use std::{
    fs::OpenOptions,
    os::fd::OwnedFd,
    sync::Mutex,
    time::{Duration, Instant},
};

use smithay::{
    backend::{
        allocator::Fourcc,
        drm::{DrmDevice, DrmDeviceFd, DrmError, DrmEvent, DrmNode, NodeType, PlaneConfig, PlaneState},
    },
    reexports::calloop::EventLoop,
    reexports::drm::control::{Device as _, ModeTypeFlags, connector},
    utils::DeviceFd,
    utils::{Rectangle, Transform},
};

static DEVICE: Mutex<()> = Mutex::new(());

#[test]
#[ignore = "requires SMITHAY_KMS_TEST_DEVICE; acquires master and resets only that explicit device"]
fn atomic_device_master_contention_pause_resume_and_final_release() {
    let _device = DEVICE.lock().unwrap();
    let path = std::env::var_os("SMITHAY_KMS_TEST_DEVICE")
        .expect("SMITHAY_KMS_TEST_DEVICE must name an isolated or authorized KMS node");
    assert_eq!(DrmNode::from_path(&path).unwrap().ty(), NodeType::Primary);
    let open = || {
        DeviceFd::from(OwnedFd::from(
            OpenOptions::new().read(true).write(true).open(&path).unwrap(),
        ))
    };
    let master = DrmDeviceFd::new_master(open()).unwrap();
    assert!(
        DrmDeviceFd::new_master(open()).is_err(),
        "a competing file must not become an unprivileged owner"
    );
    let (mut device, notifier) = DrmDevice::new_atomic(master, true).unwrap();
    assert!(device.is_atomic());
    device.pause();
    let contender = DrmDeviceFd::new_master(open()).unwrap();
    assert!(matches!(device.activate(true), Err(DrmError::DrmMasterFailed)));
    assert!(!device.is_active());
    drop(contender);
    device.activate(true).unwrap();
    drop(device);
    drop(notifier);
    drop(DrmDeviceFd::new_master(open()).expect("final device/notifier release must return DRM master"));
}

#[test]
#[ignore = "requires SMITHAY_KMS_TEST_DEVICE; commits only to that explicit isolated or authorized device"]
fn dropping_an_atomic_surface_releases_its_pending_mode_blob() {
    let _device = DEVICE.lock().unwrap();
    let path = std::env::var_os("SMITHAY_KMS_TEST_DEVICE")
        .expect("SMITHAY_KMS_TEST_DEVICE must name an isolated or authorized KMS node");
    assert_eq!(DrmNode::from_path(&path).unwrap().ty(), NodeType::Primary);
    for (paused, refresh) in [(false, false), (true, false), (false, true)] {
        let fd = DeviceFd::from(OwnedFd::from(
            OpenOptions::new().read(true).write(true).open(&path).unwrap(),
        ));
        let (mut device, notifier) =
            DrmDevice::new_atomic(DrmDeviceFd::new_master(fd).unwrap(), true).unwrap();
        let resources = device.resource_handles().unwrap();
        let connector = resources
            .connectors()
            .iter()
            .map(|handle| device.get_connector(*handle, true).unwrap())
            .find(|connector| connector.state() == connector::State::Connected)
            .expect("test device needs a connected output");
        let mode = *connector
            .modes()
            .iter()
            .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| connector.modes().first())
            .unwrap();
        let crtc = connector
            .encoders()
            .iter()
            .flat_map(|handle| resources.filter_crtcs(device.get_encoder(*handle).unwrap().possible_crtcs()))
            .next()
            .unwrap();
        let surface = device.create_surface(crtc, mode, &[connector.handle()]).unwrap();
        let (width, height) = mode.size();
        let buffer = device
            .create_dumb_buffer((u32::from(width), u32::from(height)), Fourcc::Xrgb8888, 32)
            .unwrap();
        let framebuffer = device.add_framebuffer(&buffer, 24, 32).unwrap();
        let plane = PlaneState {
            handle: surface.plane(),
            config: Some(PlaneConfig {
                src: Rectangle::from_size((f64::from(width), f64::from(height)).into()),
                dst: Rectangle::from_size((i32::from(width), i32::from(height)).into()),
                transform: Transform::Normal,
                alpha: 1.,
                damage_clips: None,
                fb: framebuffer,
                fence: None,
            }),
        };
        surface.test_state([plane.clone()], true).unwrap();
        surface.commit([plane.clone()], false).unwrap();
        let properties = device.get_properties(crtc).unwrap();
        let mode_blob = properties
            .iter()
            .find_map(|(handle, value)| {
                (device.get_property(*handle).unwrap().name().to_bytes() == b"MODE_ID").then_some(*value)
            })
            .unwrap();
        assert_ne!(mode_blob, 0);
        assert!(device.get_property_blob(mode_blob).is_ok());
        let mut event_loop = EventLoop::<bool>::try_new().unwrap();
        event_loop
            .handle()
            .insert_source(notifier, |event, _, done| match event {
                DrmEvent::VBlank(_) => *done = true,
                event => panic!("unexpected DRM event: {event:?}"),
            })
            .unwrap();
        if refresh {
            surface.reset_state().unwrap();
            surface.page_flip([plane], true).unwrap();
            let mut done = false;
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done {
                assert!(Instant::now() < deadline, "page flip did not complete");
                event_loop.dispatch(Duration::from_millis(20), &mut done).unwrap();
            }
            let properties = device.get_properties(crtc).unwrap();
            let after = properties
                .iter()
                .find_map(|(handle, value)| {
                    (device.get_property(*handle).unwrap().name().to_bytes() == b"MODE_ID").then_some(*value)
                })
                .unwrap();
            assert_eq!(
                after, mode_blob,
                "refreshing current state must not allocate a replacement kernel mode for page flips"
            );
        }
        let other = if paused {
            device.pause();
            let fd = DeviceFd::from(OwnedFd::from(
                OpenOptions::new().read(true).write(true).open(&path).unwrap(),
            ));
            Some(DrmDevice::new_atomic(DrmDeviceFd::new_master(fd).unwrap(), true).unwrap())
        } else {
            None
        };
        drop(surface);
        let observer = other.as_ref().map_or(&device, |(device, _)| device);
        assert!(
            observer.get_property_blob(mode_blob).is_err(),
            "surface destruction leaked the user reference to its mode blob (paused: {paused})"
        );
        if let Some((mut other, notifier)) = other {
            other.pause();
            drop(other);
            drop(notifier);
        }
        device.destroy_framebuffer(framebuffer).unwrap();
        device.destroy_dumb_buffer(buffer).unwrap();
        drop(device);
        drop(event_loop);
    }
}
