use crate::*;

pub const IMG_MODAL_HEIGHT: u32 = 300;
pub const IMG_MODAL_WIDTH: u32 = 296;
#[derive(Debug)]
pub struct Image {
    pub action_conn: xous::CID,
    pub action_opcode: u32,
    pub bitmap: Option<Bitmap>,
}

impl Image {
    pub fn new(action_conn: xous::CID, action_opcode: u32) -> Self {
        Image {
            action_conn,
            action_opcode,
            bitmap: None,
        }
    }
    pub fn set_bitmap(&mut self, setting: Option<Bitmap>) {
        self.bitmap = setting;
    }
}

impl ActionApi for Image {
    fn set_action_opcode(&mut self, op: u32) {
        self.action_opcode = op
    }
    fn height(&self, _glyph_height: i16, margin: i16) -> i16 {
        let bm_height = match &self.bitmap {
            Some(bm) => bm.bound.br.y - bm.bound.tl.y,
            None => 0,
        };

        log::info!("bitmap height {:?} : margin {}", bm_height, margin);
        // the modals routine always tries to center the image within a box of a given height.
        // make this height consistent with what the target is
        //margin * 2 + bm_height
        IMG_MODAL_HEIGHT as i16
    }
    fn redraw(&self, _at_height: i16, modal: &Modal) {
        if self.bitmap.is_some() {
            //bm.translate(Point::new(0, at_height));
            log::info!("drawing bitmap");
            modal
                .gam
                .draw_bitmap(modal.canvas, self.bitmap.as_ref().unwrap())
                .expect("couldn't draw bitmap");
        }
    }
    fn key_action(&mut self, k: char) -> (Option<ValidatorErr>, bool) {
        log::trace!("key_action: {}", k);
        match k {
            '\u{0}' => {
                // ignore null messages
            }
            _ => {
                send_message(
                    self.action_conn,
                    xous::Message::new_scalar(self.action_opcode as usize, 0, 0, 0, 0),
                )
                .expect("couldn't pass on dismissal");
                return (None, true);
            }
        }
        (None, false)
    }
}
