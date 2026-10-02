// SPDX-License-Identifier: GPL-3.0-or-later
//! Desktop audio adapter. No application or Slint state lives here.

use std::{io, sync::Arc};

use client_runtime::{AudioBackend, CaptureDiagnostics, CaptureInfo, OpenedCapture};
use voice_core::audio::Render;
use voice_core::pipeline::AudioProcessor;

pub struct DesktopAudio;

struct Diagnostics(voice_core::wasapi::CaptureDiagnostics);

impl CaptureDiagnostics for Diagnostics {
    fn snapshot(&self) -> CaptureInfo {
        CaptureInfo {
            opened: self.0.has_opened(),
            name: self.0.device_name(),
            is_virtual: self.0.is_virtual(),
            silent_ratio: self.0.silent_ratio(),
        }
    }
}

impl AudioBackend for DesktopAudio {
    fn capture(&self, device: Option<&str>) -> io::Result<OpenedCapture> {
        // The factory and first read both run on the capture thread. WASAPI's
        // COM objects are also used and released on that same thread.
        let stream = voice_core::wasapi::WasapiCapture::new(device.map(str::to_owned));
        let diagnostics = stream.diagnostics();
        Ok(OpenedCapture {
            stream: Box::new(stream),
            diagnostics: Some(Arc::new(Diagnostics(diagnostics))),
        })
    }

    fn render(&self, device: Option<&str>) -> io::Result<Box<dyn Render>> {
        Ok(Box::new(voice_core::wasapi::WasapiRender::new(
            device.map(str::to_owned),
        )))
    }

    fn processor(&self) -> Option<Box<dyn AudioProcessor>> {
        use voice_core::apm::{Apm, ApmConfig};
        Apm::new(voice_core::audio::SAMPLE_RATE, ApmConfig::default())
            .ok()
            .map(|apm| Box::new(apm) as Box<dyn AudioProcessor>)
    }
}
