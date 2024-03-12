use crate::{
    font, point, size, Bounds, DevicePixels, Font, FontFeatures, FontId, FontMetrics, FontRun,
    FontStyle, FontWeight, GlyphId, LineLayout, Pixels, PlatformTextSystem, Point,
    RenderGlyphParams, ShapedGlyph, SharedString, Size,
};
use anyhow::{anyhow, Context, Ok, Result};
use collections::{HashMap, HashSet};
use cosmic_text::Font as CosmicTextFont;
use cosmic_text::{
    fontdb::Query, Attrs, AttrsList, BufferLine, CacheKey, Family, FontSystem, SwashCache,
};
use fontdb::Stretch;
use itertools::Itertools;
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use pathfinder_geometry::{
    rect::{RectF, RectI},
    vector::{Vector2F, Vector2I},
};
use smallvec::SmallVec;
use std::{borrow::Cow, path::PathBuf, sync::Arc};
use util::{defer, ResultExt};
use windows::{
    core::Interface,
    Win32::{
        Foundation::{GetLastError, HWND, LPARAM},
        Graphics::{
            DirectWrite::{
                DWriteCreateFactory, IDWriteFactory, IDWriteFontFile, IDWriteFontFileLoader,
                IDWriteGdiInterop, IDWriteLocalFontFileLoader, DWRITE_FACTORY_TYPE_ISOLATED,
            },
            Gdi::{
                CreateFontIndirectW, DeleteObject, EnumFontFamiliesExW, GetDC, SelectObject,
                DEFAULT_CHARSET, LOGFONTW, TEXTMETRICW, TRUETYPE_FONTTYPE,
            },
        },
    },
};

pub(crate) struct WindowsTextSystem(RwLock<WindowsTextSystemState>);

struct WindowsTextSystemState {
    swash_cache: SwashCache,
    font_system: FontSystem,
    system_font_paths: HashSet<String>,
    fonts: Vec<Arc<CosmicTextFont>>,
    font_selections: HashMap<Font, FontId>,
    font_ids_by_family_name: HashMap<SharedString, SmallVec<[FontId; 4]>>,
    postscript_names_by_font_id: HashMap<FontId, String>,
}

unsafe fn collect_system_font_path(
    state: &mut WindowsTextSystemState,
    log_font_ptr: *const LOGFONTW,
    _text_metric_ptr: *const TEXTMETRICW,
) -> Result<()> {
    let hdc = GetDC(HWND::default());
    let hfont = CreateFontIndirectW(log_font_ptr);

    if hfont.is_invalid() {
        GetLastError()?;
    }
    let _cleanup_hfont = defer(|| {
        DeleteObject(hfont);
    });

    let dwrite_factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_ISOLATED)?;
    let gdi_interop: IDWriteGdiInterop = dwrite_factory.GetGdiInterop()?;

    let prev_selection = SelectObject(hdc, hfont);
    let _clean_select_object = defer(|| {
        SelectObject(hdc, prev_selection);
    });

    let font_face = gdi_interop.CreateFontFaceFromHdc(hdc)?;

    let mut files_len = 0;
    font_face.GetFiles(&mut files_len, None)?;
    let mut font_files: Vec<Option<IDWriteFontFile>> = vec![None; files_len as usize];
    font_face.GetFiles(&mut files_len, Some(font_files.as_mut_ptr()))?;

    for font_file in font_files {
        if let Some(font_file) = font_file {
            if let Some(file_path) = get_font_file_path(font_file).log_err() {
                state.system_font_paths.insert(file_path.into());
            }
        }
    }

    Ok(())
}

unsafe fn get_font_file_path(font_file: IDWriteFontFile) -> Result<String> {
    let mut font_file_key = std::ptr::null_mut();
    let mut font_file_key_size = 0;
    font_file.GetReferenceKey(&mut font_file_key, &mut font_file_key_size)?;
    let mut loader: IDWriteFontFileLoader = font_file.GetLoader()?;
    let mut local_loader: IDWriteLocalFontFileLoader = loader.cast()?;
    let file_path_len =
        local_loader.GetFilePathLengthFromKey(font_file_key, font_file_key_size)? as usize;
    let mut file_path: Vec<u16> = vec![0; file_path_len + 1]; // extra for NUL character
    local_loader.GetFilePathFromKey(font_file_key, font_file_key_size, &mut file_path)?;
    Ok(String::from_utf16(&file_path[0..file_path_len])?)
}

unsafe extern "system" fn load_system_font_enum_fn(
    log_font_ptr: *const LOGFONTW,
    text_metric_ptr: *const TEXTMETRICW,
    font_type: u32,
    user_data: LPARAM,
) -> i32 {
    if font_type != TRUETYPE_FONTTYPE {
        return 1;
    }

    let mut state = (user_data.0 as *mut WindowsTextSystemState)
        .as_mut()
        .unwrap();

    collect_system_font_path(&mut state, log_font_ptr, text_metric_ptr).log_err();

    1
}

impl WindowsTextSystem {
    pub(crate) fn new() -> Self {
        let mut font_system = FontSystem::new();
        Self(RwLock::new(WindowsTextSystemState {
            font_system,
            swash_cache: SwashCache::new(),
            fonts: Vec::new(),
            font_selections: HashMap::default(),
            // font_ids_by_postscript_name: HashMap::default(),
            font_ids_by_family_name: HashMap::default(),
            postscript_names_by_font_id: HashMap::default(),
        }));

        this.load_ui_icons_font();
        this.load_system_fonts();

        // @HACK: will remove, just debugging
        let icon_font = font("Segoe Fluent Icons");
        let icon_font_id = this.font_id(&icon_font).log_err();
        // if icon_font_id.log_err().is_none() {
        //     let mut state = this.0.get_mut();
        //     let segoe_ui_path: PathBuf =
        //         format!("{}\\Fonts\\SegoeIcons.ttf", env!("windir")).into();

        //     state
        //         .font_system
        //         .db_mut()
        //         .load_font_file(segoe_ui_path)
        //         .log_err();

        //     let mut font_system = &mut state.font_system;
        //     let faces: Vec<&FaceInfo> = font_system.db().faces().collect();
        //     'a: for face in faces {
        //         let face_id = face.id.clone();
        //         for (fam, _) in &face.families {
        //             if fam == "Segoe Fluent Icons" {
        //                 dbg!("FOUND IT");
        //                 let icon_font_rc = font_system.get_font(face_id).unwrap();
        //                 state
        //                     .font_selections
        //                     .insert(icon_font.clone(), FontId(state.fonts.len()));
        //                 state.fonts.push(icon_font_rc);
        //                 break 'a;
        //             }
        //         }
        //     }
        // }

        this
    }

    fn load_system_fonts(&mut self) {
        // todo(windows) make font loading non-blocking
        let mut state = self.0.get_mut();

        state.system_font_paths.clear();

        unsafe {
            EnumFontFamiliesExW(
                GetDC(HWND::default()),
                &LOGFONTW {
                    // only these 3 fields matter
                    // https://learn.microsoft.com/en-us/windows/win32/api/wingdi/nf-wingdi-enumfontfamiliesexw
                    lfCharSet: DEFAULT_CHARSET,
                    lfFaceName: [0; 32],
                    lfPitchAndFamily: 0,
                    ..LOGFONTW::default()
                },
                Some(load_system_font_enum_fn),
                LPARAM(state as *mut WindowsTextSystemState as isize),
                0,
            )
        };

        let font_db = state.font_system.db_mut();

        for font_path in &state.system_font_paths {
            font_db.load_font_file(font_path).log_err();
            println!("Loaded Font {}", font_path);
        }
    }

    fn load_ui_icons_font(&mut self) {
        let mut state = self.0.get_mut();
        let segoe_ui_path: PathBuf = format!("{}\\Fonts\\SegoeIcons.ttf", env!("windir")).into();
        let locale = state.font_system.locale();

        state.font_system.db_mut().push_face_info(FaceInfo {
            id: ID::dummy(),
            source: Source::File(segoe_ui_path),
            index: 0,
            families: vec![(
                String::from("Segoe Fluent Icons"),
                cosmic_text::fontdb::Language::English_UnitedStates, // TODO
            )],
            post_script_name: String::from("Segoe Fluent Icons"),
            style: Style::Normal,
            weight: cosmic_text::Weight::NORMAL,
            stretch: cosmic_text::Stretch::Normal,
            monospaced: false,
        });
        let face_id = state
            .font_system
            .get_font_matches(Attrs::new().family(cosmic_text::Family::Name("Segoe Fluent Icons")));
        let icon_font_rc = state
            .font_system
            .get_font(face_id.first().unwrap().clone())
            .unwrap();
        state
            .font_selections
            .insert(font("Segoe Fluent Icons"), FontId(state.fonts.len()));
        state.fonts.push(icon_font_rc);
    }
}

impl Default for WindowsTextSystem {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(unused)]
impl PlatformTextSystem for WindowsTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.0.write().add_fonts(fonts)
    }

    // todo(windows) ensure that this integrates with platform font loading
    // do we need to do more than call load_system_fonts()?
    fn all_font_names(&self) -> Vec<String> {
        self.0
            .read()
            .font_system
            .db()
            .faces()
            .map(|face| face.post_script_name.clone())
            .collect()
    }

    fn all_font_families(&self) -> Vec<String> {
        self.0
            .read()
            .font_system
            .db()
            .faces()
            .filter_map(|face| Some(face.families.get(0)?.0.clone()))
            .collect_vec()
    }

    fn font_id(&self, font: &Font) -> Result<FontId> {
        // todo(windows): Do we need to use CosmicText's Font APIs? Can we consolidate this to use font_kit?
        let lock = self.0.upgradable_read();
        if let Some(font_id) = lock.font_selections.get(font) {
            Ok(*font_id)
        } else {
            let mut lock = RwLockUpgradableReadGuard::upgrade(lock);
            let candidates = if let Some(font_ids) = lock.font_ids_by_family_name.get(&font.family)
            {
                font_ids.as_slice()
            } else {
                let font_ids = lock.load_family(&font.family, font.features)?;
                lock.font_ids_by_family_name
                    .insert(font.family.clone(), font_ids);
                lock.font_ids_by_family_name[&font.family].as_ref()
            };

            let id = lock
                .font_system
                .db()
                .query(&Query {
                    families: &[Family::Name(&font.family)],
                    weight: font.weight.into(),
                    style: font.style.into(),
                    stretch: Default::default(),
                })
                .context("no font")?;

            let font_id = if let Some(font_id) = lock.fonts.iter().position(|font| font.id() == id)
            {
                FontId(font_id)
            } else {
                // Font isn't in fonts so add it there, this is because we query all the fonts in the db
                // and maybe we haven't loaded it yet
                let font_id = FontId(lock.fonts.len());
                let font = lock.font_system.get_font(id).unwrap();
                lock.fonts.push(font);
                font_id
            };

            lock.font_selections.insert(font.clone(), font_id);
            Ok(font_id)
        }
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        let metrics = self.0.read().fonts[font_id.0].as_swash().metrics(&[]);

        FontMetrics {
            units_per_em: metrics.units_per_em as u32,
            ascent: metrics.ascent,
            descent: -metrics.descent, // todo(windows) confirm this is correct
            line_gap: metrics.leading,
            underline_position: metrics.underline_offset,
            underline_thickness: metrics.stroke_size,
            cap_height: metrics.cap_height,
            x_height: metrics.x_height,
            // todo(windows): Compute this correctly
            bounding_box: Bounds {
                origin: point(0.0, 0.0),
                size: size(metrics.max_width, metrics.ascent + metrics.descent),
            },
        }
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        let lock = self.0.read();
        let metrics = lock.fonts[font_id.0].as_swash().metrics(&[]);
        let glyph_metrics = lock.fonts[font_id.0].as_swash().glyph_metrics(&[]);
        let glyph_id = glyph_id.0 as u16;
        // todo(windows): Compute this correctly
        // see https://github.com/servo/font-kit/blob/master/src/loaders/freetype.rs#L614-L620
        Ok(Bounds {
            origin: point(0.0, 0.0),
            size: size(
                glyph_metrics.advance_width(glyph_id),
                glyph_metrics.advance_height(glyph_id),
            ),
        })
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        self.0.read().advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.read().glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.0.write().raster_bounds(params)
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.0.write().rasterize_glyph(params, raster_bounds)
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        self.0.write().layout_line(text, font_size, runs)
    }

    // todo(windows) Confirm that this has been superseded by the LineWrapper
    fn wrap_line(
        &self,
        text: &str,
        font_id: FontId,
        font_size: Pixels,
        width: Pixels,
    ) -> Vec<usize> {
        unimplemented!()
    }
}

impl WindowsTextSystemState {
    #[profiling::function]
    fn add_fonts(&mut self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        let db = self.font_system.db_mut();
        for bytes in fonts {
            match bytes {
                Cow::Borrowed(embedded_font) => {
                    db.load_font_data(embedded_font.to_vec());
                }
                Cow::Owned(bytes) => {
                    db.load_font_data(bytes);
                }
            }
        }
        Ok(())
    }

    #[profiling::function]
    fn load_family(
        &mut self,
        name: &SharedString,
        _features: FontFeatures,
    ) -> Result<SmallVec<[FontId; 4]>> {
        let mut font_ids = SmallVec::new();
        let family = self
            .font_system
            .get_font_matches(Attrs::new().family(cosmic_text::Family::Name(name)));
        for font in family.as_ref() {
            let font = self.font_system.get_font(*font).unwrap();
            // TODO: figure out why this is causing fluent icons from loading
            // if font.as_swash().charmap().map('m') == 0 {
            //     self.font_system.db_mut().remove_face(font.id());
            //     continue;
            // };

            let font_id = FontId(self.fonts.len());
            font_ids.push(font_id);
            self.fonts.push(font);
        }
        Ok(font_ids)
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        let width = self.fonts[font_id.0]
            .as_swash()
            .glyph_metrics(&[])
            .advance_width(glyph_id.0 as u16);
        let height = self.fonts[font_id.0]
            .as_swash()
            .glyph_metrics(&[])
            .advance_height(glyph_id.0 as u16);
        Ok(Size { width, height })
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        let glyph_id = self.fonts[font_id.0].as_swash().charmap().map(ch);
        if glyph_id == 0 {
            None
        } else {
            Some(GlyphId(glyph_id.into()))
        }
    }

    fn is_emoji(&self, font_id: FontId) -> bool {
        // todo(windows): implement this correctly
        self.postscript_names_by_font_id
            .get(&font_id)
            .map_or(false, |postscript_name| {
                postscript_name == "AppleColorEmoji"
            })
    }

    // todo(windows) both raster functions have problems because I am not sure this is the correct mapping from cosmic text to gpui system
    fn raster_bounds(&mut self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let font = &self.fonts[params.font_id.0];
        let font_system = &mut self.font_system;
        let image = self
            .swash_cache
            .get_image(
                font_system,
                CacheKey::new(
                    font.id(),
                    params.glyph_id.0 as u16,
                    (params.font_size * params.scale_factor).into(),
                    (0.0, 0.0),
                )
                .0,
            )
            .clone()
            .unwrap();
        Ok(Bounds {
            origin: point(image.placement.left.into(), (-image.placement.top).into()),
            size: size(image.placement.width.into(), image.placement.height.into()),
        })
    }

    #[profiling::function]
    fn rasterize_glyph(
        &mut self,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if glyph_bounds.size.width.0 == 0 || glyph_bounds.size.height.0 == 0 {
            Err(anyhow!("glyph bounds are empty"))
        } else {
            // todo(windows) handle subpixel variants
            let bitmap_size = glyph_bounds.size;
            let font = &self.fonts[params.font_id.0];
            let font_system = &mut self.font_system;
            let image = self
                .swash_cache
                .get_image(
                    font_system,
                    CacheKey::new(
                        font.id(),
                        params.glyph_id.0 as u16,
                        (params.font_size * params.scale_factor).into(),
                        (0.0, 0.0),
                    )
                    .0,
                )
                .clone()
                .unwrap();

            Ok((bitmap_size, image.data))
        }
    }

    // todo(windows) This is all a quick first pass, maybe we should be using cosmic_text::Buffer
    #[profiling::function]
    fn layout_line(&mut self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        let mut attrs_list = AttrsList::new(Attrs::new());
        let mut offs = 0;
        for run in font_runs {
            // todo(windows) We need to check we are doing utf properly
            let font = &self.fonts[run.font_id.0];
            let font = self.font_system.db().face(font.id()).unwrap();
            attrs_list.add_span(
                offs..offs + run.len,
                Attrs::new()
                    .family(Family::Name(&font.families.first().unwrap().0))
                    .stretch(font.stretch)
                    .style(font.style)
                    .weight(font.weight),
            );
            offs += run.len;
        }
        let mut line = BufferLine::new(text, attrs_list, cosmic_text::Shaping::Advanced);
        let layout = line.layout(
            &mut self.font_system,
            font_size.0,
            f32::MAX, // todo(windows) we don't have a width cause this should technically not be wrapped I believe
            cosmic_text::Wrap::None,
        );
        let mut runs = Vec::new();
        // todo(windows) what I think can happen is layout returns possibly multiple lines which means we should be probably working with it higher up in the text rendering
        let layout = layout.first().unwrap();
        for glyph in &layout.glyphs {
            let font_id = glyph.font_id;
            let font_id = FontId(
                self.fonts
                    .iter()
                    .position(|font| font.id() == font_id)
                    .unwrap(),
            );
            let mut glyphs = SmallVec::new();
            // todo(windows) this is definitely wrong, each glyph in glyphs from cosmic-text is a cluster with one glyph, ShapedRun takes a run of glyphs with the same font and direction
            glyphs.push(ShapedGlyph {
                id: GlyphId(glyph.glyph_id as u32),
                position: point((glyph.x).into(), glyph.y.into()),
                index: glyph.start,
                is_emoji: self.is_emoji(font_id),
            });
            runs.push(crate::ShapedRun { font_id, glyphs });
        }
        LineLayout {
            font_size,
            width: layout.w.into(),
            ascent: layout.max_ascent.into(),
            descent: layout.max_descent.into(),
            runs,
            len: text.len(),
        }
    }
}

impl From<RectF> for Bounds<f32> {
    fn from(rect: RectF) -> Self {
        Bounds {
            origin: point(rect.origin_x(), rect.origin_y()),
            size: size(rect.width(), rect.height()),
        }
    }
}

impl From<RectI> for Bounds<DevicePixels> {
    fn from(rect: RectI) -> Self {
        Bounds {
            origin: point(DevicePixels(rect.origin_x()), DevicePixels(rect.origin_y())),
            size: size(DevicePixels(rect.width()), DevicePixels(rect.height())),
        }
    }
}

impl From<Vector2I> for Size<DevicePixels> {
    fn from(value: Vector2I) -> Self {
        size(value.x().into(), value.y().into())
    }
}

impl From<RectI> for Bounds<i32> {
    fn from(rect: RectI) -> Self {
        Bounds {
            origin: point(rect.origin_x(), rect.origin_y()),
            size: size(rect.width(), rect.height()),
        }
    }
}

impl From<Point<u32>> for Vector2I {
    fn from(size: Point<u32>) -> Self {
        Vector2I::new(size.x as i32, size.y as i32)
    }
}

impl From<Vector2F> for Size<f32> {
    fn from(vec: Vector2F) -> Self {
        size(vec.x(), vec.y())
    }
}

impl From<FontWeight> for cosmic_text::Weight {
    fn from(value: FontWeight) -> Self {
        cosmic_text::Weight(value.0 as u16)
    }
}

impl From<FontStyle> for cosmic_text::Style {
    fn from(style: FontStyle) -> Self {
        match style {
            FontStyle::Normal => cosmic_text::Style::Normal,
            FontStyle::Italic => cosmic_text::Style::Italic,
            FontStyle::Oblique => cosmic_text::Style::Oblique,
        }
    }
}
