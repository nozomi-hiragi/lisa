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
    commands.insert_resource(Breadcrumb(tree.path(node)));
    commands.insert_resource(MenuBreadcrumb(tree.path(node)));
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
