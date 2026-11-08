use wayland_server::protocol::wl_buffer::WlBuffer;

use super::{BufferAccessError, BufferData, with_buffer_contents_mut, wl_bytes_per_pixel};

/// Failure to copy a packed image into a client SHM buffer.
#[derive(Debug, thiserror::Error)]
pub enum BufferCopyError {
    /// The source layout, format or dimensions do not fit the destination.
    #[error("image layout does not fit the SHM buffer")]
    InvalidLayout,
    /// The client buffer could not be accessed.
    #[error(transparent)]
    Access(#[from] BufferAccessError),
}

/// Copy an image into the top-left of a client SHM buffer without exposing its mapping.
///
/// `layout` describes `source`: width and height are in pixels, and offset and stride
/// are in bytes. The formats must match; this operation does not convert colors or
/// transform the image. Destination pixels outside the image and row padding are
/// preserved. All source and destination bounds are checked before writing.
///
/// Mapping access retains the SHM handler's SIGBUS protection. No Rust references
/// are created into client-controlled shared memory.
pub fn copy_buffer_contents(
    buffer: &WlBuffer,
    source: &[u8],
    layout: BufferData,
) -> Result<(), BufferCopyError> {
    with_buffer_contents_mut(buffer, |ptr, len, destination| {
        // SAFETY: The pool provides writable mapped storage for this closure,
        // protected against concurrent remapping and client-induced SIGBUS.
        unsafe { copy_rows(ptr, len, destination, source, layout) }
    })?
}

// SAFETY: `destination` must point to writable mapped storage of `len` bytes.
unsafe fn copy_rows(
    destination: *mut u8,
    len: usize,
    target: BufferData,
    source: &[u8],
    layout: BufferData,
) -> Result<(), BufferCopyError> {
    let invalid = || BufferCopyError::InvalidLayout;
    let bpp = wl_bytes_per_pixel(wayland_server::WEnum::Value(layout.format));
    if bpp == 0
        || layout.format != target.format
        || layout.width <= 0
        || layout.height <= 0
        || layout.width > target.width
        || layout.height > target.height
    {
        return Err(invalid());
    }
    let row = (layout.width as usize)
        .checked_mul(bpp as usize)
        .ok_or_else(invalid)?;
    let rows = layout.height as usize;
    let extent = |data: BufferData, length: usize| {
        let offset = usize::try_from(data.offset).ok()?;
        let stride = usize::try_from(data.stride).ok()?;
        if stride < row {
            return None;
        }
        let end = offset
            .checked_add(stride.checked_mul(rows - 1)?)?
            .checked_add(row)?;
        (end <= length).then_some((offset, stride))
    };
    let (input_offset, input_stride) = extent(layout, source.len()).ok_or_else(invalid)?;
    let (output_offset, output_stride) = extent(target, len).ok_or_else(invalid)?;
    for y in 0..rows {
        // SAFETY: Both complete image extents were checked above, including
        // overflow, before the first write. The safe source slice cannot alias
        // client memory, which the SHM API never exposes as a reference.
        unsafe {
            std::ptr::copy_nonoverlapping(
                source.as_ptr().add(input_offset + y * input_stride),
                destination.add(output_offset + y * output_stride),
                row,
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wayland_server::protocol::wl_shm::Format;

    fn layout(offset: i32, stride: i32, width: i32, height: i32) -> BufferData {
        BufferData {
            offset,
            width,
            height,
            stride,
            format: Format::Argb8888,
        }
    }

    #[test]
    fn copies_offset_rows_and_preserves_other_pixels_padding_and_pool_neighbors() {
        let mut pool = [0x55; 80];
        let source: Vec<_> = (0..40).collect();
        // SAFETY: The entire output array remains writable for this call.
        unsafe {
            copy_rows(
                pool.as_mut_ptr(),
                pool.len(),
                layout(8, 20, 4, 3),
                &source,
                layout(3, 12, 2, 2),
            )
        }
        .unwrap();
        let mut expected = [0x55; 80];
        expected[8..16].copy_from_slice(&source[3..11]);
        expected[28..36].copy_from_slice(&source[15..23]);
        assert_eq!(pool, expected);
    }

    #[test]
    fn pool_mapping_copies_bytes_and_reports_client_truncation_without_crashing() {
        use super::super::pool::Pool;
        use std::{num::NonZeroUsize, os::unix::fs::FileExt};

        let file = tempfile::tempfile().unwrap();
        file.set_len(4096).unwrap();
        let pool = Pool::new(file.try_clone().unwrap().into(), NonZeroUsize::new(4096).unwrap()).unwrap();
        let write = || {
            pool.with_data_mut(|ptr, len| {
                // SAFETY: The pool mapping remains valid and protected from SIGBUS
                // for this closure, including after the client truncates its file.
                unsafe { copy_rows(ptr, len, layout(8, 4, 1, 1), &[1, 2, 3, 4], layout(0, 4, 1, 1)) }
            })
        };
        write().unwrap().unwrap();
        let mut bytes = [0; 4];
        file.read_exact_at(&mut bytes, 8).unwrap();
        assert_eq!(bytes, [1, 2, 3, 4]);
        file.set_len(0).unwrap();
        assert!(write().is_err());
    }

    #[test]
    fn invalid_layouts_fail_before_modifying_any_destination_bytes() {
        for (input, output, bytes) in [
            (layout(0, 8, 2, 2), layout(4, 8, 2, 2), 15),
            (layout(0, 8, 2, 2), layout(28, 8, 2, 2), 32),
            (layout(0, 4, 2, 2), layout(0, 8, 2, 2), 32),
            (layout(0, 8, 2, 2), layout(0, 4, 2, 2), 32),
            (layout(0, 8, 2, 2), layout(0, 8, 1, 2), 32),
            (layout(0, 8, 2, 2), layout(0, 8, 2, 1), 32),
            (layout(-1, 8, 2, 2), layout(0, 8, 2, 2), 32),
            (layout(0, 8, 2, 2), layout(-1, 8, 2, 2), 32),
            (layout(0, 8, 2, 0), layout(0, 8, 2, 2), 32),
            (
                layout(i32::MAX, i32::MAX, 2, i32::MAX),
                layout(0, i32::MAX, 2, i32::MAX),
                32,
            ),
            (
                BufferData {
                    format: Format::Xrgb8888,
                    ..layout(0, 8, 2, 2)
                },
                layout(0, 8, 2, 2),
                32,
            ),
        ] {
            let mut pool = [0x55; 32];
            // SAFETY: The entire output array remains writable for this call.
            let result =
                unsafe { copy_rows(pool.as_mut_ptr(), pool.len(), output, &[0; 32][..bytes], input) };
            assert!(matches!(result, Err(BufferCopyError::InvalidLayout)));
            assert_eq!(pool, [0x55; 32]);
        }
    }
}
