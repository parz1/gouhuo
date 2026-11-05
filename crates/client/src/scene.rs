// SPDX-License-Identifier: GPL-3.0-or-later

//! 篝火画面的驱动：谁坐哪块石头、画面多大、火烧多快。画法本身在 `campfire.rs`。

use super::*;

/// 篝火的火苗多久动一下。10 帧：像素画本来就是一跳一跳的，再快看不出区别，
/// 只是多花 CPU —— 而这个窗口是要挂在游戏旁边一晚上的。
pub(crate) const FIRE_FRAME: std::time::Duration = std::time::Duration::from_millis(100);

/// 窗口不在前台时（比如放在副屏上）火慢下来：4 帧，看得出在烧就够了。
pub(crate) const FIRE_FRAME_IDLE: std::time::Duration = std::time::Duration::from_millis(250);

/// 把频道里的人排到篝火的座位上。
///
/// 跟名单不同，**不整个重建模型**，只改变了的那几块石头：整个换掉的话
/// 每次有人改个静音，所有石头都要销毁重建，说话的光晕动画也跟着断。
pub(crate) fn refresh_campfire(
    app: &App,
    state: &Arc<Mutex<State>>,
    channel: u32,
    me: u32,
    here: Vec<Seat>,
) {
    let ids: Vec<u32> = here.iter().map(|s| s.id as u32).collect();
    let (seated, waiting): (Vec<Option<u32>>, Vec<u32>) = {
        let mut locked = state.lock().expect("state poisoned");
        locked.seats.update(channel, me, &ids);
        let seated = (0..campfire::SEATS)
            .map(|n| {
                ids.iter()
                    .copied()
                    .find(|&id| locked.seats.seat_of(id) == Some(n))
            })
            .collect();
        (seated, locked.seats.waiting().to_vec())
    };

    let find = |id: u32| here.iter().find(|seat| seat.id as u32 == id).cloned();
    let size = CAMPFIRE.with(|stage| {
        let mut stage = stage.borrow_mut();
        if stage.set_channel(channel) {
            app.set_campfire_backdrop(ui_timing::measure("scene channel still", || stage.still()));
        }
        stage.size()
    });
    let seats: Vec<Seat> = seated
        .into_iter()
        .enumerate()
        .map(|(n, id)| {
            let mut seat = id.and_then(find).unwrap_or_default();
            if seat.present {
                (seat.stone, seat.stone_talking) =
                    campfire::stone_images(seat.seed as u32, n, size);
            }
            seat
        })
        .collect();
    let waiting: Vec<Seat> = waiting.into_iter().filter_map(find).collect();

    SlintAdapter::apply_scene(app, seats, waiting);
}

/// 篝火画面的大小变了：底图和石头都按新尺寸重画（一格的逻辑大小不变，格数变了）。
pub(crate) fn resize_campfire(app: &App, width: f32, height: f32) {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return;
    }
    let backdrop = CAMPFIRE.with(|stage| {
        let mut stage = stage.borrow_mut();
        stage.set_size((width, height)).then(|| {
            ui_timing::mark_frame(ui_timing::FramePhase::SceneResize);
            ui_timing::measure("scene resize still", || stage.still())
        })
    });
    let Some(backdrop) = backdrop else {
        return;
    };
    app.set_campfire_backdrop(backdrop);
    let geometry: Vec<SeatGeometry> = campfire::seat_geometry((width, height))
        .into_iter()
        .map(|(x, y, size, up)| SeatGeometry { x, y, size, up })
        .collect();
    app.set_scene_geometry(ModelRc::new(VecModel::from(geometry)));
    let seats = app.get_seats();
    for n in 0..seats.row_count() {
        let Some(mut seat) = seats.row_data(n) else {
            continue;
        };
        if seat.present {
            (seat.stone, seat.stone_talking) =
                campfire::stone_images(seat.seed as u32, n, (width, height));
            seats.set_row_data(n, seat);
        }
    }
}

/// 火现在该多久动一下；`None` 是别动。
///
/// - 篝火不在眼前（收在托盘里、最小化、切到文字聊天、窄窗口时看着频道树）：不动
/// - 系统关了动画：不动
/// - 窗口在前台：[`FIRE_FRAME`]
/// - 不在前台，但前台是个铺满同一块屏幕的全屏程序（就是在打游戏）：不动。
///   这是最常见的情况 —— 篝火开着没最小化、被游戏整个盖住，画了也没人看得见
/// - 不在前台、也没被全屏盖住（比如放在副屏上）：慢下来，[`FIRE_FRAME_IDLE`]
pub(crate) fn fire_pace(app: &App) -> Option<std::time::Duration> {
    // 两堆火：频道里的那堆，和首页上的那堆。哪个都不在眼前就不动。
    if !(app.get_campfire_shown() || app.get_home_shown())
        || !app.window().is_visible()
        || app.window().is_minimized()
        || !system_animations_on()
    {
        return None;
    }
    match foreground(app) {
        Foreground::Us => Some(FIRE_FRAME),
        Foreground::FullscreenOther => None,
        Foreground::Other => Some(FIRE_FRAME_IDLE),
    }
}

/// 火的定时器。一开始是停着的，由状态同步那边（`spawn_status_poll`）按需开起来；
/// 该停、该变快变慢，它每一帧自己看（`fire_pace`）。
pub(crate) fn spawn_fire(weak: slint::Weak<App>) {
    let timer = slint::Timer::default();
    let mut last = std::time::Instant::now();
    timer.start(slint::TimerMode::Repeated, FIRE_FRAME, move || {
        let Some(app) = weak.upgrade() else { return };
        let now = std::time::Instant::now();
        // 定时器停过一阵再开，别让火一下子「快进」好几秒
        let dt = now
            .duration_since(last)
            .min(FIRE_FRAME_IDLE * 2)
            .as_secs_f32();
        last = now;
        let pace = fire_pace(&app);
        FIRE_TIMER.with(|slot| {
            if let Some(timer) = slot.borrow().as_ref() {
                match pace {
                    None => timer.stop(),
                    Some(pace) if timer.interval() != pace => timer.set_interval(pace),
                    Some(_) => {}
                }
            }
        });
        if pace.is_none() {
            return;
        }
        if app.get_home_shown() {
            let frame = HOME_FIRE.with(|stage| stage.borrow_mut().advance(dt));
            app.set_home_fire(frame);
        } else {
            let frame = CAMPFIRE.with(|stage| stage.borrow_mut().advance(dt));
            app.set_campfire_backdrop(frame);
        }
    });
    timer.stop();
    FIRE_TIMER.with(|slot| *slot.borrow_mut() = Some(timer));
}
