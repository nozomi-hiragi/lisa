//! ファイルのプレビューを「カートリッジ」として差し替え・追加できる仕組み。

pub mod cartridges;

use std::collections::HashMap;

use bevy::math::Rect;
use bevy::prelude::*;

use crate::render::shapes::{Painter, over};

/// カートリッジが紙の内側を描くための材料。
pub struct DrawContext<'a, 'b, 'w, 's> {
    pub painter: &'a mut Painter<'b, 'w, 's>,
    pub name: &'a str,
    /// 紙の内側の四角。
    pub inner: Rect,
    /// 紙の色（背景に重ねた結果）。
    pub page: Color,
    /// 紙の元の色。
    pub paper: Color,
    pub z: f32,
}

/// ファイルのプレビューを描くカートリッジ。
pub trait Cartridge: Send + Sync + 'static {
    /// カートリッジの識別子（例: `"image"`）。
    fn id(&self) -> &'static str;
    /// 担当する拡張子（小文字）。空なら受け皿（どれにも合わないファイル）。
    fn extensions(&self) -> &'static [&'static str];
    /// 同じ拡張子に複数あるときの優先度（高い方が選ばれる）。
    fn priority(&self) -> i32 {
        0
    }
    /// 紙の内側を描く。
    fn draw(&self, ctx: &mut DrawContext<'_, '_, '_, '_>);
}

/// 受け皿のカートリッジが 1 つも登録されていないときの組み込みの受け皿。
struct BuiltinFallback;

static BUILTIN_FALLBACK: BuiltinFallback = BuiltinFallback;

impl Cartridge for BuiltinFallback {
    fn id(&self) -> &'static str {
        "fallback"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[]
    }

    fn draw(&self, _ctx: &mut DrawContext<'_, '_, '_, '_>) {}
}

/// 登録されたカートリッジと、拡張子ごとに選ばれたカートリッジの表。
#[derive(Resource, Default)]
pub struct PreviewRegistry {
    cartridges: Vec<Box<dyn Cartridge>>,
    /// 拡張子（小文字）→ 選ばれたカートリッジの番号。
    by_ext: HashMap<String, usize>,
    /// 受け皿（`extensions()` が空）のカートリッジの番号。
    fallback: Option<usize>,
    /// 拡張子（小文字）→ カートリッジ id の上書き（設定から入れる口）。
    overrides: HashMap<String, String>,
}

impl PreviewRegistry {
    /// カートリッジを追加する。同じ拡張子なら優先度の高い方（同点なら先に追加した方）が選ばれる。
    pub fn add(&mut self, c: impl Cartridge) {
        let idx = self.cartridges.len();
        self.cartridges.push(Box::new(c));
        let prio = self.cartridges[idx].priority();
        let exts = self.cartridges[idx].extensions();
        if exts.is_empty() {
            let replace = self
                .fallback
                .is_none_or(|prev| prio > self.cartridges[prev].priority());
            if replace {
                self.fallback = Some(idx);
            }
        } else {
            for &ext in exts {
                let key = ext.to_ascii_lowercase();
                let replace = self
                    .by_ext
                    .get(&key)
                    .is_none_or(|&prev| prio > self.cartridges[prev].priority());
                if replace {
                    self.by_ext.insert(key, idx);
                }
            }
        }
    }

    /// 特定の拡張子で使うカートリッジを id で上書きする。
    #[allow(dead_code)]
    pub fn set_override(&mut self, ext: impl Into<String>, id: impl Into<String>) {
        self.overrides
            .insert(ext.into().to_ascii_lowercase(), id.into());
    }

    /// ファイル名に合うカートリッジを選ぶ。
    /// 優先順位: 上書き指定 → 拡張子が合うもの（priority が高い方） → 受け皿 → 組み込みの受け皿。
    pub fn pick(&self, name: &str) -> &dyn Cartridge {
        let ext = extension(name).to_ascii_lowercase();
        if let Some(id) = self.overrides.get(&ext)
            && let Some(c) = self.cartridges.iter().find(|c| c.id() == id)
        {
            return c.as_ref();
        }
        if let Some(&idx) = self.by_ext.get(&ext) {
            return self.cartridges[idx].as_ref();
        }
        if let Some(idx) = self.fallback {
            return self.cartridges[idx].as_ref();
        }
        &BUILTIN_FALLBACK
    }
}

/// 拡張子（無ければ空）。
pub fn extension(name: &str) -> &str {
    name.rsplit_once('.').map_or("", |(_, ext)| ext)
}

/// 紙の形の背景（右上が折れている）と、選んだカートリッジの絵を描く。`background` は下の色。
pub fn draw(
    registry: &PreviewRegistry,
    painter: &mut Painter,
    name: &str,
    frame: Rect,
    background: Color,
    z: f32,
) {
    let paper = Color::srgb(0.85, 0.87, 0.95);
    let page = over(background, paper, 0.10);
    let fold = over(background, paper, 0.22);
    let c = frame.height() * 0.15;
    painter.rect(
        Rect::new(frame.min.x, frame.min.y, frame.max.x - c, frame.max.y),
        page,
        z,
    );
    painter.rect(
        Rect::new(frame.max.x - c, frame.min.y, frame.max.x, frame.max.y - c),
        page,
        z,
    );
    painter.triangle(
        [
            Vec2::new(frame.max.x - c, frame.max.y),
            Vec2::new(frame.max.x - c, frame.max.y - c),
            Vec2::new(frame.max.x, frame.max.y - c),
        ],
        fold,
        z,
    );

    let pad = frame.height() * 0.14;
    let inner = Rect {
        min: frame.min + pad,
        max: frame.max - pad,
    };
    let mut ctx = DrawContext {
        painter,
        name,
        inner,
        page,
        paper,
        z: z + 0.05,
    };
    registry.pick(name).draw(&mut ctx);
}

/// プレビューのカートリッジを登録するプラグイン。
pub struct PreviewPlugin;

impl Plugin for PreviewPlugin {
    fn build(&self, app: &mut App) {
        let mut registry = PreviewRegistry::default();
        cartridges::register(&mut registry);
        app.insert_resource(registry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dummy {
        id: &'static str,
        exts: &'static [&'static str],
        priority: i32,
    }

    impl Cartridge for Dummy {
        fn id(&self) -> &'static str {
            self.id
        }

        fn extensions(&self) -> &'static [&'static str] {
            self.exts
        }

        fn priority(&self) -> i32 {
            self.priority
        }

        fn draw(&self, _ctx: &mut DrawContext<'_, '_, '_, '_>) {}
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        let mut r = PreviewRegistry::default();
        cartridges::register(&mut r);
        assert_eq!(r.pick("photo.jpg").id(), "image");
        assert_eq!(r.pick("PHOTO.JPG").id(), "image");
        assert_eq!(r.pick("Screen.PnG").id(), "image");
        assert_eq!(r.pick("thumb.webp").id(), "image");
        assert_eq!(r.pick("THUMB.WEBP").id(), "image");
        assert_eq!(r.pick("README.MD").id(), "text");
        assert_eq!(r.pick("clip.MOV").id(), "video");
    }

    #[test]
    fn unknown_extension_goes_to_fallback() {
        let mut r = PreviewRegistry::default();
        cartridges::register(&mut r);
        assert_eq!(r.pick("archive.zip").id(), "other");
        assert_eq!(r.pick("Makefile").id(), "other");

        // 受け皿が無くても panic せず、組み込みの Fallback を使う。
        let empty = PreviewRegistry::default();
        assert_eq!(empty.pick("archive.zip").id(), "fallback");
        assert_eq!(empty.pick("Makefile").id(), "fallback");
    }

    #[test]
    fn higher_priority_wins_and_tie_keeps_first() {
        let mut r = PreviewRegistry::default();
        r.add(Dummy {
            id: "low",
            exts: &["png"],
            priority: 0,
        });
        r.add(Dummy {
            id: "high",
            exts: &["png"],
            priority: 10,
        });
        r.add(Dummy {
            id: "tie",
            exts: &["png"],
            priority: 10,
        });
        assert_eq!(r.pick("a.png").id(), "high");

        // 先に高い方を足しても、同点なら先の方が勝つ。
        let mut r2 = PreviewRegistry::default();
        r2.add(Dummy {
            id: "first",
            exts: &["png"],
            priority: 5,
        });
        r2.add(Dummy {
            id: "second",
            exts: &["png"],
            priority: 5,
        });
        r2.add(Dummy {
            id: "lower",
            exts: &["png"],
            priority: 1,
        });
        assert_eq!(r2.pick("a.png").id(), "first");
    }

    #[test]
    fn override_beats_priority() {
        let mut r = PreviewRegistry::default();
        r.add(Dummy {
            id: "low",
            exts: &["png"],
            priority: 0,
        });
        r.add(Dummy {
            id: "high",
            exts: &["png"],
            priority: 100,
        });
        assert_eq!(r.pick("a.png").id(), "high");

        r.set_override("PNG", "low");
        assert_eq!(r.pick("a.png").id(), "low");
        assert_eq!(r.pick("A.PNG").id(), "low");

        // 存在しない id の上書きは無視して通常の優先順位に戻る。
        r.set_override("png", "missing");
        assert_eq!(r.pick("a.png").id(), "high");
    }
}
