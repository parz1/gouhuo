// SPDX-License-Identifier: GPL-3.0-or-later
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use test_bot::{audio, network::Direction, run_bot, Config};

const HELP: &str = "gouhuo-bot <invite> [options]
  --invite-file FILE       Read an invitation without placing it in shell history
  --room NAME_OR_ID        Join an existing channel; no temporary rooms yet
  --count N               Independent bots, 1..256 (default 1)
  --ramp-ms N             Delay between bot starts, 0..1000 (default 100)
  --speakers N            Only first N bots transmit; others receive (default all)
  --seconds N             Duration per connected bot, 0<N<=86400 (default 60)
  --name PREFIX           Nickname prefix (default bot)
  --play FILE.wav         Loop 16-bit PCM WAV, mono/stereo, 8–96 kHz
  --echo                  Echo received mixed audio with ~200 ms delay; count=1
  --silent                Receive only (mutually exclusive with play/echo)
  --gain N                Input gain 0..1 (default 1)
  --loss PERCENT          Loss per direction, 0..100
  --delay-ms N            One-way delay, 0..5000
  --jitter-ms N           Gaussian jitter stddev, 0..5000
  --seed N                Repeat random decisions for the same input packet stream
  --outage-at N           UDP outage start in seconds after bot voice startup
  --outage-seconds N      Outage duration; TCP stays connected
  --outage-direction up|down|both (default both)
  --log FILE.jsonl        New diagnostic file; otherwise JSONL goes to stdout
  --help                  Show this help
No microphone, speaker, GUI or persisted identity is used.";

struct Args {
    cfg: Config,
    log: Option<PathBuf>,
    ramp: Duration,
    speakers: Option<usize>,
}

fn invalid(text: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, text)
}
fn duration(value: &str) -> io::Result<Duration> {
    let n = value
        .parse::<f64>()
        .map_err(|_| invalid("invalid duration"))?;
    if !n.is_finite() || !(0.0..=86400.0).contains(&n) {
        return Err(invalid(
            "duration must be finite and within 0..86400 seconds",
        ));
    }
    Ok(Duration::from_secs_f64(n))
}

fn parse(args: impl IntoIterator<Item = String>) -> io::Result<Args> {
    let mut cfg = Config::new(String::new());
    let mut args = args.into_iter();
    let mut log = None;
    let mut invite_file = None;
    let mut play = None;
    let mut ramp = Duration::from_millis(100);
    let mut speakers = None;
    while let Some(flag) = args.next() {
        if flag == "--echo" {
            cfg.echo = true;
            continue;
        }
        if flag == "--silent" {
            cfg.silent = true;
            continue;
        }
        if !flag.starts_with('-') && cfg.invite.is_empty() {
            cfg.invite = flag;
            continue;
        }
        let value = args.next().ok_or_else(|| invalid("missing option value"))?;
        match flag.as_str() {
            "--invite-file" => invite_file = Some(PathBuf::from(value)),
            "--room" => cfg.room = Some(value),
            "--count" => cfg.count = value.parse().map_err(|_| invalid("invalid count"))?,
            "--speakers" => {
                speakers = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| invalid("invalid speaker count"))?,
                )
            }
            "--ramp-ms" => {
                let ms = value.parse::<u64>().map_err(|_| invalid("invalid ramp"))?;
                if ms > 1000 {
                    return Err(invalid("ramp must be within 0..1000 ms"));
                }
                ramp = Duration::from_millis(ms);
            }
            "--seconds" => cfg.seconds = duration(&value)?,
            "--name" => cfg.name = value,
            "--play" => play = Some(PathBuf::from(value)),
            "--gain" => cfg.gain = value.parse().map_err(|_| invalid("invalid gain"))?,
            "--loss" => {
                cfg.impairment.netem.loss =
                    value.parse::<f64>().map_err(|_| invalid("invalid loss"))? / 100.0
            }
            "--delay-ms" => {
                cfg.impairment.netem.owd_ms = value.parse().map_err(|_| invalid("invalid delay"))?
            }
            "--jitter-ms" => {
                cfg.impairment.netem.jitter_ms =
                    value.parse().map_err(|_| invalid("invalid jitter"))?
            }
            "--seed" => {
                cfg.impairment.netem.seed = value.parse().map_err(|_| invalid("invalid seed"))?
            }
            "--outage-at" => cfg.impairment.outage_at = duration(&value)?,
            "--outage-seconds" => cfg.impairment.outage_for = duration(&value)?,
            "--outage-direction" => {
                cfg.impairment.direction = match value.as_str() {
                    "up" => Direction::Up,
                    "down" => Direction::Down,
                    "both" => Direction::Both,
                    _ => return Err(invalid("outage direction must be up, down or both")),
                }
            }
            "--log" => log = Some(PathBuf::from(value)),
            _ => return Err(invalid("unknown option; use --help")),
        }
    }
    if let Some(path) = invite_file {
        if !cfg.invite.is_empty() {
            return Err(invalid("choose an invitation or --invite-file"));
        }
        use std::io::Read;
        let mut text = String::new();
        std::fs::File::open(path)?
            .take(65537)
            .read_to_string(&mut text)?;
        if text.len() > 65536 {
            return Err(invalid("invitation file exceeds 64 KiB"));
        }
        cfg.invite = text.trim().to_owned();
    }
    if cfg.invite.is_empty() {
        return Err(invalid("an invitation is required; use --help"));
    }
    if play.is_some() && (cfg.echo || cfg.silent) {
        return Err(invalid("play, echo and silent are mutually exclusive"));
    }
    if let Some(path) = play {
        cfg.samples = audio::load_wav(&path)?;
    }
    cfg.validate()?;
    if speakers.is_some_and(|n| n == 0 || n > cfg.count) || (speakers.is_some() && cfg.silent) {
        return Err(invalid(
            "speakers must be within 1..count and cannot accompany silent",
        ));
    }
    Ok(Args {
        cfg,
        log,
        ramp,
        speakers,
    })
}

fn run(args: Args) -> io::Result<()> {
    let mut output: Box<dyn Write> = match args.log {
        Some(path) => Box::new(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?,
        ),
        None => Box::new(io::stdout()),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::sync_channel(1024);
    let mut workers = Vec::new();
    let epoch = std::time::Instant::now();
    for index in 0..args.cfg.count {
        let mut cfg = args.cfg.clone();
        if args.speakers.is_some_and(|n| index >= n) {
            cfg.silent = true;
        }
        let tx = tx.clone();
        let stop = Arc::clone(&stop);
        let start_at = epoch + args.ramp * index as u32;
        workers.push(std::thread::spawn(move || {
            while std::time::Instant::now() < start_at {
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                std::thread::sleep(
                    start_at
                        .saturating_duration_since(std::time::Instant::now())
                        .min(Duration::from_millis(20)),
                );
            }
            let result = run_bot(cfg, index, tx.clone(), Arc::clone(&stop));
            if let Err(e) = &result {
                stop.store(true, Ordering::Relaxed);
                let _ = tx.send(
                    serde_json::json!({"event":"error", "bot":index + 1, "error":e.to_string()}),
                );
            }
            result
        }));
    }
    drop(tx);
    let mut write_result = Ok(());
    for message in &rx {
        write_result = (|| {
            serde_json::to_writer(&mut output, &message)?;
            writeln!(output)?;
            output.flush()
        })();
        if write_result.is_err() {
            stop.store(true, Ordering::Relaxed);
            break;
        }
    }
    drop(rx); // Unblocks producers if output failed.
    let mut bot_result = Ok(());
    for worker in workers {
        match worker.join() {
            Ok(Err(e)) => bot_result = Err(e),
            Err(_) => bot_result = Err(io::Error::other("bot worker panicked")),
            _ => {}
        }
    }
    write_result.and(bot_result)
}

fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|s| s == "--help" || s == "-h") {
        println!("{HELP}");
        return std::process::ExitCode::SUCCESS;
    }
    match parse(args).and_then(run) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("gouhuo-bot: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_cli_values_never_start_a_session() {
        for options in [
            vec!["--seconds", "NaN"],
            vec!["--seconds", "-1"],
            vec!["--count", "0"],
            vec!["--ramp-ms", "1001"],
            vec!["--speakers", "0"],
            vec!["--speakers", "2"],
            vec!["--silent", "--speakers", "1"],
            vec!["--loss", "101"],
            vec!["--jitter-ms", "inf"],
            vec!["--echo", "--count", "2"],
            vec!["--echo", "--silent"],
            vec!["--unknown", "x"],
            vec!["--count"],
        ] {
            let args = std::iter::once("unused-invite".to_owned())
                .chain(options.into_iter().map(str::to_owned));
            assert!(parse(args).is_err());
        }
    }
}
