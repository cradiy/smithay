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

#[test]
#[ignore = "requires SMITHAY_RENDER_TEST_DEVICE; uses only the selected render node without KMS master"]
fn submitted_fence_covers_readback_commands_issued_after_the_render_frame() {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use smithay::backend::renderer::{ExportMem, Offscreen, gles::GlesTexture};
    let path = std::env::var_os("SMITHAY_RENDER_TEST_DEVICE").unwrap();
    assert_eq!(DrmNode::from_path(&path).unwrap().ty(), NodeType::Render);
    let fd = DeviceFd::from(OwnedFd::from(
        OpenOptions::new().read(true).write(true).open(path).unwrap(),
    ));
    let (mut renderer, _gbm) = GlesRenderer::from_gbm_fd(fd).unwrap();
    let mut texture: GlesTexture = renderer.create_buffer(Fourcc::Argb8888, (8, 8).into()).unwrap();
    let mut target = renderer.bind(&mut texture).unwrap();
    let mut frame = renderer
        .render(&mut target, (8, 8).into(), Transform::Normal)
        .unwrap();
    frame
        .clear(
            Color32F::new(1., 0., 0., 1.),
            &[Rectangle::from_size((8, 8).into())],
        )
        .unwrap();
    frame
        .clear(
            Color32F::new(0., 0., 1., 1.),
            &[Rectangle::from_size((4, 4).into())],
        )
        .unwrap();
    let _render_fence = frame.finish().unwrap();
    let mapping = renderer
        .copy_framebuffer(&target, Rectangle::from_size((8, 8).into()), Fourcc::Argb8888)
        .unwrap();
    let completion = renderer.submit().unwrap();
    let fd = completion
        .export()
        .expect("validation requires a native GPU fence");
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    assert_eq!(
        poll(
            &mut fds,
            Some(&Timespec {
                tv_sec: 2,
                tv_nsec: 0
            })
        )
        .unwrap(),
        1
    );
    assert!(fds[0].revents().contains(PollFlags::IN));
    assert!(completion.is_reached());
    let pixels: Vec<_> = renderer
        .map_texture(&mapping)
        .unwrap()
        .as_chunks::<4>()
        .0
        .iter()
        .copied()
        .map(u32::from_ne_bytes)
        .collect();
    assert_eq!(pixels.iter().filter(|p| **p == 0xffff0000).count(), 48);
    assert_eq!(pixels.iter().filter(|p| **p == 0xff0000ff).count(), 16);
}
