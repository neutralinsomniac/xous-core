use gam::*;
use num_traits::*;
use locales::t;
use crate::api::*;
use xous_ipc::String;

pub(crate) fn pddb_menu(conn: xous::CID) {
    let mut menu_items = Vec::<MenuItem>::new();

    menu_items.push(
        MenuItem {
            name: String::from_str(t!("pddb.menu.listbasis", xous::LANG)),
            action_conn: Some(conn),
            action_opcode: Opcode::MenuListBasis.to_u32().unwrap(),
            action_payload: MenuPayload::Scalar([0, 0, 0, 0]),
            close_on_select: true,
        }
    );
    menu_items.push(MenuItem {
        name: String::from_str(t!("mainmenu.closemenu", xous::LANG)),
        action_conn: None,
        action_opcode: 0,
        action_payload: MenuPayload::Scalar([0, 0, 0, 0]),
        close_on_select: true,
    });

    menu_matic(menu_items, PDDB_MENU_NAME, None);
}