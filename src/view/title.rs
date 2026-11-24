//! 今いる場所を、ウィンドウのタイトルとメニューバーに出す（デバッグ用の FPS も添える）。

use super::*;

/// 今いる場所（Root からの道のり）。
#[derive(Resource)]
pub(crate) struct Breadcrumb(pub(crate) String);

/// タイトルの後ろに添える文字（デバッグ用）。None なら何も添えない。
#[derive(Resource, Default)]
pub struct TitleSuffix(pub Option<String>);

/// 今いる場所を、ウィンドウのタイトルとメニューバーに出す。
pub(crate) fn show_breadcrumb(commands: &mut Commands, tree: &FileTree, node: NodeId) {
    let parts = crate::model::display_path::tree_parts(tree, node);
    let text = lisa_core::display_path::text(&parts);
    let steps = crate::model::display_path::steps(tree, node).unwrap_or_default();
    commands.insert_resource(Breadcrumb(text.clone()));
    let marker_status = tree.marker_view_status(node).map(str::to_owned);
    commands.insert_resource(MenuBreadcrumb {
        text,
        parts,
        steps,
        marker_status,
    });
}

/// 木が作り直されると、登録前の項目を見分ける印（`Step::Attached`）も変わる。
/// 今の場所のパンくずと正の場所を、今の木から取り直す。
pub(crate) fn refresh_breadcrumb_steps(
    tree: Res<FileTree>,
    nav: Res<Nav>,
    breadcrumb: Option<ResMut<MenuBreadcrumb>>,
    mut location: ResMut<Location>,
) {
    if !tree.is_changed() {
        return;
    }
    let node = nav.top().node;
    let Some(steps) = crate::model::display_path::steps(&tree, node) else {
        return;
    };
    if let Some(mut breadcrumb) = breadcrumb {
        if breadcrumb.steps != steps && breadcrumb.parts.len() == steps.len() {
            breadcrumb.steps = steps;
        }
        let status = tree.marker_view_status(node);
        if breadcrumb.marker_status.as_deref() != status {
            breadcrumb.marker_status = status.map(str::to_owned);
        }
    }
    if matches!(*location, Location::Attached(_)) {
        let fresh = Location::from_tree(&tree, node);
        if *location != fresh {
            *location = fresh;
        }
    }
}

/// ウィンドウのタイトル。今いる場所に、あればデバッグ情報を添える。変わったときだけ書き換える。
pub(crate) fn update_title(
    breadcrumb: Res<Breadcrumb>,
    debug: Option<Res<TitleSuffix>>,
    mut window: Single<&mut Window, With<PrimaryWindow>>,
) {
    let title = match debug.as_deref() {
        Some(TitleSuffix(Some(suffix))) => format!("Lisa — {}  |  {suffix}", breadcrumb.0),
        _ => format!("Lisa — {}", breadcrumb.0),
    };
    if window.title != title {
        window.title = title;
    }
}
