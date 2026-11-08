//! Native Wayland night light. Dropping our controls restores the original LUT.
use std::{
    collections::HashMap,
    fs::File,
    io::{self, Seek, Write},
    os::fd::{AsFd, FromRawFd},
};
use wayland_client::{protocol::wl_output::WlOutput, Dispatch, QueueHandle};
use wayland_protocols_wlr::gamma_control::v1::client::{
    zwlr_gamma_control_manager_v1 as manager, zwlr_gamma_control_v1 as control,
};

struct Control {
    object: control::ZwlrGammaControlV1,
    size: Option<u32>,
    failed: bool,
}
impl Drop for Control {
    fn drop(&mut self) {
        self.object.destroy();
    }
}
#[derive(Default)]
pub struct Gamma {
    pub manager: Option<manager::ZwlrGammaControlManagerV1>,
    controls: HashMap<u32, Control>,
    temperature: Option<u32>,
    pub error: String,
}
impl Gamma {
    pub fn supported(&self) -> bool {
        self.manager.is_some()
    }
    pub fn active(&self) -> bool {
        self.temperature.is_some()
            && self
                .controls
                .values()
                .any(|c| c.size.is_some() && !c.failed)
    }
    pub fn sync<D>(
        &mut self,
        outputs: Vec<(u32, WlOutput)>,
        temperature: Option<u32>,
        qh: &QueueHandle<D>,
    ) where
        D: Dispatch<control::ZwlrGammaControlV1, u32> + 'static,
    {
        self.controls
            .retain(|id, _| outputs.iter().any(|(i, _)| id == i));
        if temperature.is_none() {
            self.controls.clear();
            self.temperature = None;
            self.error.clear();
            return;
        }
        let Some(manager) = &self.manager else {
            self.error = "컴포지터가 야간 모드 색온도 제어를 지원하지 않습니다".into();
            return;
        };
        let changed = self.temperature != temperature;
        self.temperature = temperature;
        for (id, output) in outputs {
            self.controls.entry(id).or_insert_with(|| Control {
                object: manager.get_gamma_control(&output, qh, id),
                size: None,
                failed: false,
            });
        }
        if changed {
            self.apply_all();
        }
    }
    fn apply_all(&mut self) {
        let Some(temperature) = self.temperature else {
            return;
        };
        for c in self.controls.values_mut().filter(|c| !c.failed) {
            if let Some(size) = c.size {
                match ramp_file(size, temperature) {
                    Ok(fd) => c.object.set_gamma(fd.as_fd()),
                    Err(e) => {
                        c.failed = true;
                        self.error = format!("색온도 적용 실패: {e}");
                    }
                }
            }
        }
    }
    pub fn size(&mut self, id: u32, size: u32) {
        if let Some(c) = self.controls.get_mut(&id) {
            if !(2..=65536).contains(&size) {
                c.failed = true;
                self.error = "지원하지 않는 화면 감마 테이블입니다".into();
                return;
            }
            c.size = Some(size);
        }
        self.apply_all();
    }
    pub fn failed(&mut self, id: u32) {
        if let Some(c) = self.controls.get_mut(&id) {
            c.failed = true;
        }
        self.error="일부 화면에 색온도를 적용하지 못했습니다. 드라이버 지원 또는 다른 야간 모드 프로그램을 확인하세요.".into();
    }
}
fn gains(kelvin: u32) -> [f64; 3] {
    // Planck spectral ratios, normalized to a neutral 6500 K white point.
    // Only channel gains change; hardware brightness remains independent.
    let kelvin = f64::from(kelvin.clamp(2500, 6500));
    let mut rgb = [0.0; 3];
    for (value, wavelength) in rgb.iter_mut().zip([610e-9, 550e-9, 460e-9]) {
        let c = 0.014_387_768_77 / wavelength;
        *value = (c / 6500.0_f64).exp_m1() / (c / kelvin).exp_m1();
    }
    let maximum = rgb[0];
    for value in &mut rgb {
        *value = (*value / maximum).clamp(0.0, 1.0).powf(1.0 / 2.2);
    }
    rgb
}
fn ramp(size: u32, kelvin: u32) -> io::Result<Vec<u8>> {
    if !(2..=65536).contains(&size) {
        return Err(io::Error::other("invalid gamma size"));
    }
    let mut bytes = Vec::with_capacity(size as usize * 6);
    for gain in gains(kelvin) {
        for i in 0..size {
            let value = (f64::from(i) / f64::from(size - 1) * gain * 65535.0).round() as u16;
            bytes.extend_from_slice(&value.to_ne_bytes());
        }
    }
    Ok(bytes)
}
fn ramp_file(size: u32, kelvin: u32) -> io::Result<File> {
    let fd = unsafe { libc::memfd_create(c"lucy-night-light".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(&ramp(size, kelvin)?)?;
    file.rewind()?;
    Ok(file)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Client {
        gamma: Gamma,
        outputs: Vec<(u32, WlOutput)>,
    }
    impl wayland_client::Dispatch<wayland_client::protocol::wl_registry::WlRegistry, ()> for Client {
        fn event(
            s: &mut Self,
            r: &wayland_client::protocol::wl_registry::WlRegistry,
            e: wayland_client::protocol::wl_registry::Event,
            _: &(),
            _: &wayland_client::Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wayland_client::protocol::wl_registry::Event::Global {
                name,
                interface,
                version,
            } = e
            {
                match interface.as_str() {
                    "zwlr_gamma_control_manager_v1" => {
                        s.gamma.manager = Some(r.bind(name, 1, qh, ()))
                    }
                    "wl_output" => s.outputs.push((name, r.bind(name, version.min(4), qh, ()))),
                    _ => {}
                }
            }
        }
    }
    wayland_client::delegate_noop!(Client:ignore wayland_client::protocol::wl_output::WlOutput);
    wayland_client::delegate_noop!(Client:ignore manager::ZwlrGammaControlManagerV1);
    impl Dispatch<control::ZwlrGammaControlV1, u32> for Client {
        fn event(
            s: &mut Self,
            _: &control::ZwlrGammaControlV1,
            e: control::Event,
            id: &u32,
            _: &wayland_client::Connection,
            _: &QueueHandle<Self>,
        ) {
            match e {
                control::Event::GammaSize { size } => s.gamma.size(*id, size),
                control::Event::Failed => s.gamma.failed(*id),
                _ => {}
            }
        }
    }
    #[test]
    #[ignore = "briefly applies warm gamma and restores it; requires a live Wayland session"]
    fn compositor_accepts_native_gamma_and_releases_it_on_disable() {
        let connection = wayland_client::Connection::connect_to_env().unwrap();
        let mut queue = connection.new_event_queue();
        let qh = queue.handle();
        connection.display().get_registry(&qh, ());
        let mut c = Client::default();
        queue.roundtrip(&mut c).unwrap();
        queue.roundtrip(&mut c).unwrap();
        assert!(c.gamma.supported());
        assert!(!c.outputs.is_empty());
        c.gamma.sync(c.outputs.clone(), Some(4500), &qh);
        for _ in 0..10 {
            queue.roundtrip(&mut c).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let active = c.gamma.active();
        let error = c.gamma.error.clone();
        c.gamma.sync(c.outputs.clone(), None, &qh);
        queue.roundtrip(&mut c).unwrap();
        assert!(c.gamma.controls.is_empty());
        assert!(!c.gamma.active());
        assert!(active, "{error}");
        assert!(error.is_empty(), "{error}");
        println!("PASS: native gamma ramps accepted; dropping controls restored compositor color management");
    }
    #[test]
    fn neutral_is_identity_and_warmth_reduces_blue_most() {
        assert_eq!(gains(6500), [1.0; 3]);
        for k in [2500, 3500, 4500, 5500] {
            let [r, g, b] = gains(k);
            assert_eq!(r, 1.0);
            assert!(g > b && g < r && b > 0.0);
        }
    }
    #[test]
    fn ramps_are_native_endian_monotonic_and_bounded() {
        for size in [256, 1024] {
            for k in [2500, 4500, 6500] {
                let bytes = ramp(size, k).unwrap();
                assert_eq!(bytes.len(), size as usize * 6);
                for channel in bytes.chunks_exact(size as usize * 2) {
                    let values: Vec<_> = channel
                        .chunks_exact(2)
                        .map(|v| u16::from_ne_bytes([v[0], v[1]]))
                        .collect();
                    assert_eq!(values[0], 0);
                    assert!(values.windows(2).all(|v| v[0] <= v[1]));
                }
            }
        }
        assert!(ramp(0, 4500).is_err());
    }
}
