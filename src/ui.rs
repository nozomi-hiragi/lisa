//! 画面に固定の UI（メニューバーと設定のポップアップ、読み込み中のリスト、重複の一覧、カタログ、
//! ツールチップ、右クリックのメニュー）。

use crate::{catalog::CatalogView, dups::Dups, organize::OrganizeState};
use bevy::ecs::system::SystemParam;
use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::picking::events::PointerState;
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
        app.add_systems(
            Update,
            (
                sync_side_panels
                    .in_set(SidePanelSync)
                    .before(SidePanelLayout),
                scroll_side_panels.after(SidePanelLayout),
            ),
        )
        .add_systems(PostUpdate, sync_side_panel_picking);
    }
}

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SidePanelSync;

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SidePanelLayout;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidePanel {
    Dups,
    Catalog,
    Organize,
}

#[derive(Default)]
struct SidePanelOpen {
    dups: bool,
    catalog: bool,
    organize: bool,
}

#[derive(SystemParam)]
struct SidePanels<'w> {
    dups: ResMut<'w, Dups>,
    catalog: ResMut<'w, CatalogView>,
    organize: ResMut<'w, OrganizeState>,
}

impl SidePanels<'_> {
    fn open(&self) -> SidePanelOpen {
        SidePanelOpen {
            dups: self.dups.open,
            catalog: self.catalog.open,
            organize: self.organize.panel_open,
        }
    }

    fn set_open(&mut self, panel: Option<SidePanel>) {
        let dups = panel == Some(SidePanel::Dups);
        let catalog = panel == Some(SidePanel::Catalog);
        let organize = panel == Some(SidePanel::Organize);
        if self.dups.open != dups {
            self.dups.open = dups;
        }
        if self.catalog.open != catalog {
            self.catalog.open = catalog;
        }
        if self.organize.panel_open != organize {
            self.organize.panel_open = organize;
        }
    }

    fn toggle(&mut self, panel: SidePanel) {
        let is_open = match panel {
            SidePanel::Dups => self.dups.open,
            SidePanel::Catalog => self.catalog.open,
            SidePanel::Organize => self.organize.panel_open,
        };
        self.set_open((!is_open).then_some(panel));
    }
}

fn sync_side_panels(mut previous: Local<SidePanelOpen>, mut panels: SidePanels) {
    let current = panels.open();
    let opened = if !previous.organize && current.organize {
        Some(SidePanel::Organize)
    } else if !previous.catalog && current.catalog {
        Some(SidePanel::Catalog)
    } else if !previous.dups && current.dups {
        Some(SidePanel::Dups)
    } else {
        None
    };
    if let Some(panel) = opened {
        panels.set_open(Some(panel));
    }
    *previous = panels.open();
}

#[derive(Component, Default)]
struct SidePanelList {
    // 閉じるアニメーション中は表示を続けるが、スクロールは受け付けない。
    closing: bool,
}

// Pickable は親から継承されないため、閉じるパネルの子孫にも適用する。
#[derive(Component)]
struct SavedPanelPickable(Option<Pickable>);

fn sync_side_panel_picking(
    mut commands: Commands,
    lists: Query<(&SidePanelList, &ChildOf), Changed<SidePanelList>>,
    children: Query<&Children>,
    pickables: Query<(Option<&Pickable>, Option<&SavedPanelPickable>)>,
    mut pointer_state: ResMut<PointerState>,
) {
    for (list, parent) in &lists {
        let panel = parent.parent();
        for entity in std::iter::once(panel).chain(children.iter_descendants(panel)) {
            let Ok((pickable, saved)) = pickables.get(entity) else {
                continue;
            };
            if list.closing && saved.is_none() {
                // Bevy は直前のホバー先へリリースを送るため、閉じる前の押下も破棄する。
                for button in pointer_state.pointer_buttons.values_mut() {
                    button.pressing.remove(&entity);
                }
                commands
                    .entity(entity)
                    .insert((SavedPanelPickable(pickable.copied()), Pickable::IGNORE));
            } else if !list.closing
                && let Some(saved) = saved
            {
                let mut entity = commands.entity(entity);
                entity.remove::<SavedPanelPickable>();
                if let Some(pickable) = saved.0 {
                    entity.insert(pickable);
                } else {
                    entity.remove::<Pickable>();
                }
            }
        }
    }
}

fn scroll_side_panels(
    mut wheel: MessageReader<MouseWheel>,
    window: Single<&Window, With<PrimaryWindow>>,
    panels: Query<&Node>,
    debug_cursor: Option<Res<crate::input::gesture::DebugCursor>>,
    mut lists: Query<(&SidePanelList, &ChildOf, &ComputedNode, &mut ScrollPosition)>,
) {
    for event in wheel.read() {
        let cursor = if event.window == Entity::PLACEHOLDER {
            debug_cursor.as_deref().and_then(|cursor| cursor.0)
        } else {
            window.cursor_position()
        };
        if !cursor.is_some_and(|p| p.x > crate::view::view_width(&window) && p.y > menu::BAR_PX) {
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

    impl SidePanel {
        fn open_directly(self, world: &mut World) {
            match self {
                Self::Dups => world.resource_mut::<Dups>().open = true,
                Self::Catalog => world.resource_mut::<CatalogView>().open = true,
                Self::Organize => world.resource_mut::<OrganizeState>().panel_open = true,
            }
        }

        fn update_contents(self, world: &mut World) {
            match self {
                Self::Dups => world.resource_mut::<Dups>().selected = Some(1),
                Self::Catalog => world.resource_mut::<CatalogView>().selected = Some(1),
                Self::Organize => world.resource_mut::<OrganizeState>().selected_plan = Some(1),
            }
        }
    }

    fn panel_app() -> App {
        let mut app = App::new();
        app.init_resource::<Dups>()
            .init_resource::<CatalogView>()
            .init_resource::<OrganizeState>()
            .add_systems(Update, sync_side_panels);
        app
    }

    fn assert_open(world: &World, panel: Option<SidePanel>) {
        assert_eq!(
            world.resource::<Dups>().open,
            panel == Some(SidePanel::Dups)
        );
        assert_eq!(
            world.resource::<CatalogView>().open,
            panel == Some(SidePanel::Catalog)
        );
        assert_eq!(
            world.resource::<OrganizeState>().panel_open,
            panel == Some(SidePanel::Organize)
        );
    }

    #[test]
    fn side_panels_switch_in_all_directions_while_the_old_panel_updates() {
        let all = [SidePanel::Dups, SidePanel::Catalog, SidePanel::Organize];
        for from in all {
            for to in all.into_iter().filter(|to| *to != from) {
                let mut app = panel_app();
                from.open_directly(app.world_mut());
                app.update();
                assert_open(app.world(), Some(from));
                from.update_contents(app.world_mut());
                to.open_directly(app.world_mut());
                app.update();
                assert_open(app.world(), Some(to));
            }
        }
    }

    #[test]
    fn menu_toggle_switches_immediately_and_second_click_closes() {
        let all = [SidePanel::Dups, SidePanel::Catalog, SidePanel::Organize];
        for from in all {
            for to in all.into_iter().filter(|to| *to != from) {
                let mut app = panel_app();
                let click = app
                    .world_mut()
                    .register_system(move |mut panels: SidePanels| panels.toggle(to));
                from.open_directly(app.world_mut());
                app.update();
                app.world_mut().run_system(click).unwrap();
                assert_open(app.world(), Some(to));
                app.update();
                app.world_mut().run_system(click).unwrap();
                assert_open(app.world(), None);
                app.update();
                assert_open(app.world(), None);
            }
        }
    }

    #[test]
    fn closing_panel_descendants_receive_no_clicks_and_reopen_restores_picking() {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::picking::{
            InteractionPlugin, PickingPlugin,
            backend::{HitData, PointerHits},
            pointer::{Location, PointerAction, PointerId, PointerInput, PointerLocation},
        };

        #[derive(Resource, Default)]
        struct Clicks(usize);

        let mut app = App::new();
        app.add_plugins((PickingPlugin, InteractionPlugin))
            .init_resource::<Clicks>()
            .add_systems(PostUpdate, sync_side_panel_picking);
        let location = Location {
            target: NormalizedRenderTarget::None {
                width: 1,
                height: 1,
            },
            position: Vec2::ZERO,
        };
        let world = app.world_mut();
        world.spawn((PointerId::Mouse, PointerLocation::new(location.clone())));
        let panel = world.spawn_empty().id();
        let list = world.spawn((ChildOf(panel), SidePanelList::default())).id();
        let button = world
            .spawn(ChildOf(list))
            .observe(|_: On<PointerClick>, mut clicks: ResMut<Clicks>| clicks.0 += 1)
            .id();
        let decoration = world.spawn((ChildOf(button), Pickable::IGNORE)).id();
        let hit = HitData::new(Entity::PLACEHOLDER, 0.0, None, None);
        let send = |app: &mut App, action| {
            let world = app.world_mut();
            world.write_message(PointerHits::new(
                PointerId::Mouse,
                vec![(button, hit.clone())],
                0.0,
            ));
            world.write_message(PointerInput::new(
                PointerId::Mouse,
                location.clone(),
                action,
            ));
            app.update();
        };
        let press = PointerAction::Press(PointerButton::Primary);
        let release = PointerAction::Release(PointerButton::Primary);
        let click = |app: &mut App| {
            send(app, press);
            send(app, release);
        };

        click(&mut app);
        assert_eq!(app.world().resource::<Clicks>().0, 1);
        app.world_mut()
            .get_mut::<SidePanelList>(list)
            .unwrap()
            .closing = true;
        // 閉じるフレームで押下を受けても、閉じたあとにクリックは届かない。
        send(&mut app, press);
        send(&mut app, release);
        assert_eq!(app.world().resource::<Clicks>().0, 1);
        for entity in [panel, list, button, decoration] {
            assert_eq!(app.world().get::<Pickable>(entity), Some(&Pickable::IGNORE));
        }
        click(&mut app);
        assert_eq!(app.world().resource::<Clicks>().0, 1);
        app.world_mut()
            .get_mut::<SidePanelList>(list)
            .unwrap()
            .closing = false;
        app.update();
        assert_eq!(
            app.world().get::<Pickable>(decoration),
            Some(&Pickable::IGNORE)
        );
        assert!(app.world().get::<Pickable>(button).is_none());
        click(&mut app);
        assert_eq!(app.world().resource::<Clicks>().0, 2);
    }

    #[test]
    fn simultaneous_opens_prefer_organize_then_catalog() {
        let mut app = panel_app();
        for panel in [SidePanel::Dups, SidePanel::Catalog, SidePanel::Organize] {
            panel.open_directly(app.world_mut());
        }
        app.update();
        assert_open(app.world(), Some(SidePanel::Organize));

        SidePanel::Dups.open_directly(app.world_mut());
        SidePanel::Catalog.open_directly(app.world_mut());
        app.update();
        assert_open(app.world(), Some(SidePanel::Catalog));
    }
}
