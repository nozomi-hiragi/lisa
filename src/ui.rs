//! 画面に固定の UI（メニューバーと設定のポップアップ、読み込み中のリスト、重複の一覧、カタログ、
//! ツールチップ、右クリックのメニュー）。

use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::prelude::*;
use bevy::ui::ScrollPosition;
use bevy::window::PrimaryWindow;

pub mod catalog;
pub mod context_menu;
pub(crate) mod dialog;
pub mod dups;
pub mod menu;
pub mod organize;
pub mod progress;
pub mod tooltip;

pub struct SidePanelScrollPlugin;

impl Plugin for SidePanelScrollPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, scroll_side_panels.after(SidePanelLayout));
    }
}

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SidePanelLayout;

#[derive(Component, Default)]
struct SidePanelList {
    // 閉じるアニメーション中は表示を続けるが、スクロールは受け付けない。
    closing: bool,
}

fn scroll_side_panels(
    mut wheel: MessageReader<MouseWheel>,
    window: Single<&Window, With<PrimaryWindow>>,
    panels: Query<&Node>,
    mut lists: Query<(&SidePanelList, &ChildOf, &ComputedNode, &mut ScrollPosition)>,
) {
    let over_panel = window
        .cursor_position()
        .is_some_and(|p| p.x > crate::view::view_width(&window) && p.y > menu::BAR_PX);
    for event in wheel.read() {
        if !over_panel {
            continue;
        }
        for (list, parent, node, mut scroll) in &mut lists {
            if !list.closing
                && panels
                    .get(parent.parent())
                    .is_ok_and(|panel| panel.display != Display::None)
            {
                scroll_panel_list(&mut scroll, node, event, window.scale_factor());
            }
        }
    }
}

// Bevy は描画用の位置だけを下端で止めるので、入力側にも超過分を残さない。
fn scroll_panel_list(
    scroll: &mut ScrollPosition,
    node: &ComputedNode,
    event: &MouseWheel,
    scale_factor: f32,
) {
    let delta = match event.unit {
        MouseScrollUnit::Line => event.y * 24.0,
        MouseScrollUnit::Pixel => event.y / scale_factor,
    };
    // 一覧を組み直した直後は下端がまだ分からないので、上端だけで止める。
    if node.content_size.y == 0.0 {
        scroll.y = (scroll.y - delta).max(0.0);
        return;
    }
    // パネルの一覧にはボーダー・スクロールバーがない。content_size には余白の分も含まれる。
    let bottom = (node.content_size.y - node.size.y).max(0.0) * node.inverse_scale_factor;
    scroll.y = (scroll.y.clamp(0.0, bottom) - delta).clamp(0.0, bottom);
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::input::touch::TouchPhase;
    use bevy::prelude::Entity;

    #[test]
    fn panel_scroll_keeps_offset_until_content_is_laid_out() {
        let mut node = ComputedNode {
            size: bevy::math::Vec2::new(380.0, 200.0),
            ..Default::default()
        };
        let mut scroll = ScrollPosition(bevy::math::Vec2::new(0.0, 100.0));
        let mut wheel = MouseWheel {
            unit: MouseScrollUnit::Pixel,
            x: 0.0,
            y: 8.0,
            window: Entity::PLACEHOLDER,
            phase: TouchPhase::Moved,
        };

        scroll_panel_list(&mut scroll, &node, &wheel, 1.0);
        assert_eq!(scroll.y, 92.0);

        // レイアウトの結果、以前の位置が新しい下端を越えていても逆方向にすぐ動く。
        node.content_size.y = 240.0;
        scroll_panel_list(&mut scroll, &node, &wheel, 1.0);
        assert_eq!(scroll.y, 32.0);

        wheel.y = -100.0;
        scroll_panel_list(&mut scroll, &node, &wheel, 1.0);
        assert_eq!(scroll.y, 40.0);

        // 中身が一覧より短い場合は、計算済みの下端が 0 になる。
        node.content_size.y = 160.0;
        scroll_panel_list(&mut scroll, &node, &wheel, 1.0);
        assert_eq!(scroll.y, 0.0);

        node.content_size.y = 0.0;
        scroll.y = 3.0;
        wheel.y = 8.0;
        scroll_panel_list(&mut scroll, &node, &wheel, 1.0);
        assert_eq!(scroll.y, 0.0);
    }
}
