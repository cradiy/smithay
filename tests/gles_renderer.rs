use std::{fs::OpenOptions, os::fd::OwnedFd};

use smithay::{
    backend::{
        allocator::{
            Allocator, Fourcc,
            dmabuf::AsDmabuf,
            gbm::{GbmAllocator, GbmBufferFlags},
        },
        drm::{DrmNode, NodeType},
        renderer::{Bind, Color32F, Frame, Renderer, gles::GlesRenderer},
    },
    utils::{DeviceFd, Rectangle, Transform},
};

#[test]
#[ignore = "requires SMITHAY_RENDER_TEST_DEVICE; uses only the selected render node without KMS master"]
fn quiescing_the_renderer_retires_submitted_work_without_waiting_on_its_frame_fence() {
    let path = std::env::var_os("SMITHAY_RENDER_TEST_DEVICE").unwrap();
    assert_eq!(DrmNode::from_path(&path).unwrap().ty(), NodeType::Render);
    let fd = DeviceFd::from(OwnedFd::from(
        OpenOptions::new().read(true).write(true).open(path).unwrap(),
    ));
    let (mut renderer, gbm) = GlesRenderer::from_gbm_fd(fd).unwrap();
    assert!(!renderer.is_software());
    let modifiers: Vec<_> = renderer
        .egl_context()
        .display()
        .dmabuf_render_formats()
        .iter()
        .filter(|format| format.code == Fourcc::Xrgb8888)
        .map(|format| format.modifier)
        .collect();
    let mut allocator = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING);
    let buffer = allocator
        .create_buffer(512, 512, Fourcc::Xrgb8888, &modifiers)
        .unwrap();
    let mut dmabuf = buffer.export().unwrap();
    let mut target = renderer.bind(&mut dmabuf).unwrap();
    let sync = {
        let mut frame = renderer
            .render(&mut target, (512, 512).into(), Transform::Normal)
            .unwrap();
        frame
            .clear(
                Color32F::new(1., 0., 0., 1.),
                &[Rectangle::from_size((512, 512).into())],
            )
            .unwrap();
        frame.finish().unwrap()
    };
    assert!(sync.contains_fence(), "validation requires an actual GPU fence");
    renderer.quiesce().unwrap();
    assert!(
        sync.is_reached(),
        "quiesce returned while prior GPU work was still in flight"
    );
}
