use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_app_kit::{
    NSPasteboard, NSPasteboardContentsOptions, NSPasteboardItem, NSPasteboardTypeFileURL,
};
use objc2_foundation::{NSArray, NSString, NSURL};
use std::path::PathBuf;

const MAX_BACKUP_BYTES: usize = 32 * 1024 * 1024;

pub struct FilePaste {
    board: Retained<NSPasteboard>,
    saved: Vec<Retained<NSPasteboardItem>>,
    revision: isize,
}

fn write_items(board: &NSPasteboard, items: &[Retained<NSPasteboardItem>]) -> bool {
    let values = items
        .iter()
        .map(|item| ProtocolObject::from_ref(&**item))
        .collect::<Vec<_>>();
    items.is_empty() || board.writeObjects(&NSArray::from_slice(&values))
}

impl FilePaste {
    pub fn install(paths: &[PathBuf]) -> Option<Self> {
        let incoming = paths
            .iter()
            .map(|path| {
                let url =
                    NSURL::fileURLWithPath_isDirectory(&NSString::from_str(path.to_str()?), false);
                let item = NSPasteboardItem::new();
                let address = url.absoluteString()?;
                item.setString_forType(&address, unsafe { NSPasteboardTypeFileURL })
                    .then_some(item)
            })
            .collect::<Option<Vec<_>>>()?;
        let board = NSPasteboard::generalPasteboard();
        let initial = board.changeCount();
        let mut saved = Vec::new();
        let mut total = 0usize;
        for items in board.pasteboardItems().iter() {
            for item in items.iter() {
                let copy = NSPasteboardItem::new();
                for kind in item.types().iter() {
                    let data = item.dataForType(&kind)?;
                    total = total.checked_add(data.length())?;
                    if total > MAX_BACKUP_BYTES || !copy.setData_forType(&data, &kind) {
                        return None;
                    }
                }
                saved.push(copy);
            }
        }
        if board.changeCount() != initial {
            return None;
        }
        board.prepareForNewContentsWithOptions(NSPasteboardContentsOptions::CurrentHostOnly);
        let written = write_items(&board, &incoming);
        let revision = board.changeCount();
        written.then_some(Self {
            board,
            saved,
            revision,
        })
    }

    pub fn unchanged(&self) -> bool {
        self.board.changeCount() == self.revision
    }
}

impl Drop for FilePaste {
    fn drop(&mut self) {
        if self.unchanged() {
            self.board.clearContents();
            write_items(&self.board, &self.saved);
        }
    }
}
