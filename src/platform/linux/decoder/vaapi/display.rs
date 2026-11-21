//! VA-API 解码设备、会话和表面池。
use std::cell::{OnceCell, RefCell};
use std::rc::Rc;

use cros_libva::{
    Config, Context, Display, Surface, UsageHint, VAConfigAttrib, VAConfigAttribType, VAEntrypoint,
    VAImageFormat, VAProfile, VASurfaceID,
};

use crate::media::decode_api::DecodeError;

/// 解码回读使用 NV12（8 位 4:2:0）。
const RT_FORMAT_YUV420: u32 = cros_libva::VA_RT_FORMAT_YUV420;

thread_local! {
    // VA-API 设备只在工作线程内共享，避免为每个待探测的尺寸重复初始化驱动。
    static DECODE_DISPLAYS: OnceCell<Vec<(Rc<Display>, Vec<VAProfile::Type>)>> =
        const { OnceCell::new() };
}

/// Open and keep the first render node that supports H.264 encoding.
pub(super) fn encoder_display(
    codec: crate::media::encoding::Codec,
) -> Result<Rc<Display>, DecodeError> {
    thread_local! {
        static DISPLAYS: RefCell<Vec<(crate::media::encoding::Codec, Rc<Display>)>> = const { RefCell::new(Vec::new()) };
    }
    DISPLAYS.with(|cell| {
        if let Some((_, display)) = cell
            .borrow()
            .iter()
            .find(|(supported, _)| *supported == codec)
        {
            return Ok(Rc::clone(display));
        }
        crate::platform::linux::cuda::initialize_before_va();
        for index in 128..192u32 {
            let path = format!("/dev/dri/renderD{index}");
            let Some(display) = open_render_node(&path) else {
                continue;
            };
            if !supports_encode_on(&display, codec) {
                continue;
            }
            tracing::debug!(path, ?codec, "VA-API encode display opened");
            log_vendor(&display);
            cell.borrow_mut().push((codec, Rc::clone(&display)));
            return Ok(display);
        }
        Err(DecodeError::Unsupported)
    })
}

fn open_decode_displays() -> Vec<(Rc<Display>, Vec<VAProfile::Type>)> {
    crate::platform::linux::cuda::initialize_before_va();
    let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("renderD"))
                .is_some_and(|index| index.parse::<u32>().is_ok())
        })
        .collect();
    nodes.sort_unstable();
    nodes
        .iter()
        .filter_map(|path| {
            let path = path.to_str()?;
            let display = open_render_node(path)?;
            let profiles = decode_profiles(&display);
            if profiles.is_empty() {
                return None;
            }
            tracing::debug!(path, "VA-API 解码设备已打开");
            log_vendor(&display);
            Some((display, profiles))
        })
        .collect()
}

pub(super) fn decode_display(
    profile: VAProfile::Type,
    width: u32,
    height: u32,
) -> Result<Rc<Display>, DecodeError> {
    DECODE_DISPLAYS.with(|cell| {
        cell.get_or_init(open_decode_displays)
            .iter()
            .find(|(display, profiles)| {
                profiles.contains(&profile) && supports_size(display, profile, width, height)
            })
            .map(|(display, _)| Rc::clone(display))
            .ok_or(DecodeError::Unsupported)
    })
}

/// Opens one render node. libva before 2.15 cannot map the `nvidia-drm`
/// kernel driver to its VA driver, so on an NVIDIA node a failed open is
/// retried with nvidia-vaapi-driver named explicitly, as `LIBVA_DRIVER_NAME`
/// would.
fn open_render_node(path: &str) -> Option<Rc<Display>> {
    match Display::open_drm_display(path) {
        Ok(display) => Some(display),
        Err(cros_libva::OpenDrmDisplayError::VaInitialize(_)) if nvidia_node(path) => {
            let display = Display::open_drm_display_with_driver(path, Some(c"nvidia")).ok()?;
            tracing::debug!(path, "VA-API driver named for the NVIDIA render node");
            Some(display)
        }
        Err(_) => None,
    }
}

fn nvidia_node(path: &str) -> bool {
    let node = path.rsplit('/').next().unwrap_or_default();
    std::fs::read_to_string(format!("/sys/class/drm/{node}/device/vendor"))
        .is_ok_and(|vendor| vendor.trim() == "0x10de")
}

fn log_vendor(display: &Rc<Display>) {
    if let Ok(vendor) = display.query_vendor_string() {
        tracing::info!(vendor, "VA-API driver");
    }
}

/// Profiles the driver decodes, i.e. that expose the VLD entrypoint.
fn decode_profiles(display: &Rc<Display>) -> Vec<VAProfile::Type> {
    let Ok(profiles) = display.query_config_profiles() else {
        return Vec::new();
    };
    profiles
        .into_iter()
        .filter(|profile| {
            display
                .query_config_entrypoints(*profile)
                .is_ok_and(|entrypoints| entrypoints.contains(&VAEntrypoint::VAEntrypointVLD))
        })
        .collect()
}

/// 检查驱动报告的像素格式和尺寸限制。
fn supports_size(display: &Display, profile: VAProfile::Type, width: u32, height: u32) -> bool {
    let mut attributes = [
        attribute(VAConfigAttribType::VAConfigAttribRTFormat),
        attribute(VAConfigAttribType::VAConfigAttribMaxPictureWidth),
        attribute(VAConfigAttribType::VAConfigAttribMaxPictureHeight),
    ];
    if display
        .get_config_attributes(profile, VAEntrypoint::VAEntrypointVLD, &mut attributes)
        .is_err()
    {
        return false;
    }
    let value = |kind: VAConfigAttribType::Type| {
        attributes
            .iter()
            .find(|attribute| attribute.type_ == kind)
            .map(|attribute| attribute.value)
            .filter(|value| *value != cros_libva::VA_ATTRIB_NOT_SUPPORTED)
    };
    if value(VAConfigAttribType::VAConfigAttribRTFormat)
        .is_some_and(|formats| formats & RT_FORMAT_YUV420 == 0)
    {
        return false;
    }
    // 驱动未报告尺寸上限时，交由实际解码会话验证。
    let within = |kind: VAConfigAttribType::Type, wanted: u32| {
        value(kind).is_none_or(|limit| limit == 0 || limit >= wanted)
    };
    within(VAConfigAttribType::VAConfigAttribMaxPictureWidth, width)
        && within(VAConfigAttribType::VAConfigAttribMaxPictureHeight, height)
}

/// 获取支持 H.264 的设备，供硬件编码测试创建自己的配置。
#[cfg(test)]
pub(super) fn display_handle() -> Result<Rc<Display>, crate::media::decode_api::DecodeError> {
    decode_display(VAProfile::VAProfileH264High, 0, 0)
}

/// Whether the driver offers an H.264 encode entrypoint.
pub(super) fn supports_encode_on(display: &Display, codec: crate::media::encoding::Codec) -> bool {
    let profiles: &[VAProfile::Type] = match codec {
        crate::media::encoding::Codec::H264 => &[
            VAProfile::VAProfileH264High,
            VAProfile::VAProfileH264Main,
            VAProfile::VAProfileH264ConstrainedBaseline,
        ],
        crate::media::encoding::Codec::H265 => &[VAProfile::VAProfileHEVCMain],
        crate::media::encoding::Codec::Av1 => &[VAProfile::VAProfileAV1Profile0],
    };
    profiles.iter().any(|profile| {
        display
            .query_config_entrypoints(*profile)
            .is_ok_and(|entrypoints| entrypoints.contains(&VAEntrypoint::VAEntrypointEncSlice))
    })
}

const fn attribute(kind: VAConfigAttribType::Type) -> VAConfigAttrib {
    VAConfigAttrib {
        type_: kind,
        value: 0,
    }
}

/// A decode context plus the surfaces its pictures are written into.
pub(super) struct Pool {
    pub(super) context: Rc<Context>,
    pub(super) width: u32,
    pub(super) height: u32,
    /// The NV12 image format used to copy pictures out of their surfaces.
    pub(super) nv12: VAImageFormat,
    surfaces: Vec<Option<Surface<()>>>,
    ids: Vec<VASurfaceID>,
    /// Surfaces still needed as references, by index.
    reserved: Vec<bool>,
    /// Surfaces whose dmabuf the renderer still holds. The `Surface` stays in
    /// `surfaces` so its `Rc` lives on, but `take` skips the slot.
    leased: Vec<bool>,
    /// Indices the renderer released; drained before the next decode.
    returned: crate::platform::linux::dmabuf::ReturnedSurfaces,
    #[allow(dead_code, reason = "Owns the config for the context's lifetime.")]
    config: Config,
}

impl Pool {
    pub(super) fn new(
        profile: VAProfile::Type,
        width: u32,
        height: u32,
        count: usize,
    ) -> Result<Self, DecodeError> {
        let display = decode_display(profile, width, height)?;
        let config = display
            .create_config(
                vec![VAConfigAttrib {
                    type_: VAConfigAttribType::VAConfigAttribRTFormat,
                    value: RT_FORMAT_YUV420,
                }],
                profile,
                VAEntrypoint::VAEntrypointVLD,
            )
            .map_err(|error| {
                tracing::debug!(%error, "VA-API config unavailable");
                DecodeError::Unsupported
            })?;
        let surfaces = display
            .create_surfaces(
                RT_FORMAT_YUV420,
                None,
                width,
                height,
                Some(UsageHint::USAGE_HINT_DECODER),
                vec![(); count],
            )
            .map_err(|error| {
                tracing::debug!(%error, count, "VA-API surface allocation failed");
                DecodeError::HardwareFailure
            })?;
        let context = display
            .create_context(&config, width, height, Some(&surfaces), true)
            .map_err(|error| {
                tracing::debug!(%error, "VA-API context creation failed");
                DecodeError::HardwareFailure
            })?;
        let nv12 = display
            .query_image_formats()
            .map_err(|error| {
                tracing::debug!(%error, "VA-API image formats unavailable");
                DecodeError::HardwareFailure
            })?
            .into_iter()
            .find(|format| format.fourcc == cros_libva::VA_FOURCC_NV12)
            .ok_or_else(|| {
                tracing::debug!("VA-API driver offers no NV12 image format");
                DecodeError::Unsupported
            })?;
        let ids = surfaces.iter().map(Surface::id).collect();
        Ok(Self {
            context,
            width,
            height,
            nv12,
            reserved: vec![false; surfaces.len()],
            leased: vec![false; surfaces.len()],
            returned: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            surfaces: surfaces.into_iter().map(Some).collect(),
            ids,
            config,
        })
    }

    pub(super) fn id(&self, index: usize) -> VASurfaceID {
        self.ids[index]
    }

    /// Take a surface that is neither in flight nor holding a reference frame.
    pub(super) fn take(&mut self) -> Result<(usize, Surface<()>), DecodeError> {
        // Slots whose dmabuf the renderer still holds are skipped: reusing one
        // would overwrite the picture under the imported texture.
        let index = self
            .surfaces
            .iter()
            .enumerate()
            .position(|(index, surface)| {
                surface.is_some() && !self.reserved[index] && !self.leased[index]
            })
            .ok_or(DecodeError::Backend)?;
        let surface = self.surfaces[index].take().ok_or(DecodeError::Backend)?;
        Ok((index, surface))
    }

    pub(super) fn put(&mut self, index: usize, surface: Surface<()>) {
        self.surfaces[index] = Some(surface);
    }

    /// Returns a surface whose dmabuf is now held by the renderer.
    ///
    /// The slot stays occupied and is skipped by `take` until the lease drops,
    /// at which point `drain_returned` frees it for reuse.
    pub(super) fn put_leased(
        &mut self,
        index: usize,
        surface: Surface<()>,
    ) -> crate::platform::linux::dmabuf::SurfaceLease {
        self.surfaces[index] = Some(surface);
        self.leased[index] = true;
        crate::platform::linux::dmabuf::SurfaceLease::new(
            std::sync::Arc::clone(&self.returned),
            index,
        )
    }

    /// Frees the slots the renderer released since the last call.
    pub(super) fn drain_returned(&mut self) {
        let drained: Vec<usize> = match self.returned.lock() {
            Ok(mut returned) => returned.drain(..).collect(),
            Err(poisoned) => poisoned.into_inner().drain(..).collect(),
        };
        for index in drained {
            if let Some(slot) = self.leased.get_mut(index) {
                *slot = false;
            }
        }
    }

    /// Mark exactly the surfaces the reference lists still name.
    pub(super) fn reserve_only(&mut self, indices: &[usize]) {
        for (index, reserved) in self.reserved.iter_mut().enumerate() {
            *reserved = indices.contains(&index);
        }
    }

    pub(super) fn release_all(&mut self) {
        self.reserved
            .iter_mut()
            .for_each(|reserved| *reserved = false);
    }
}
