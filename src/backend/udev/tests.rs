use super::*;

#[test]
fn hotplug_admits_only_primary_nodes_on_the_selected_seat() {
    let mut devices = UdevDevices(HashMap::new());
    for (name, assigned) in [
        ("renderD128", None),
        ("card1-eDP-1", None),
        ("card", None),
        ("card1", Some(OsStr::new("seat1"))),
    ] {
        let eligible = gpu_matches_seat(OsStr::new(name), assigned, "seat0");
        assert!(
            devices
                .update(EventType::Add, 17, Some(Path::new("/dev/dri/ignored")), eligible)
                .is_none()
        );
    }
    let eligible = gpu_matches_seat(OsStr::new("card12"), None, "seat0");
    assert!(matches!(
        devices.update(EventType::Add, 17, Some(Path::new("/dev/dri/card12")), eligible),
        Some(UdevEvent::Added { device_id: 17, .. })
    ));
    assert!(
        devices
            .update(EventType::Add, 17, Some(Path::new("/dev/dri/card12")), eligible)
            .is_none(),
        "an add queued during enumeration must not publish the device twice"
    );
}

#[test]
fn seat_reassignment_removes_and_readmits_without_requiring_device_removal() {
    let mut devices = UdevDevices(HashMap::new());
    let path = Path::new("/dev/dri/card1");
    devices.update(EventType::Add, 17, Some(path), true);
    let away = gpu_matches_seat(OsStr::new("card1"), Some(OsStr::new("seat1")), "seat0");
    assert!(matches!(
        devices.update(EventType::Change, 17, Some(path), away),
        Some(UdevEvent::Removed { device_id: 17 })
    ));
    let back = gpu_matches_seat(OsStr::new("card1"), Some(OsStr::new("seat0")), "seat0");
    assert!(matches!(
        devices.update(EventType::Change, 17, Some(path), back),
        Some(UdevEvent::Added { device_id: 17, .. })
    ));
    assert!(matches!(
        devices.update(EventType::Change, 17, None, back),
        Some(UdevEvent::Changed { device_id: 17 })
    ));
    assert!(
        matches!(
            devices.update(EventType::Remove, 17, None, false),
            Some(UdevEvent::Removed { device_id: 17 })
        ),
        "removal must retire a tracked device even without seat or path metadata"
    );
}
