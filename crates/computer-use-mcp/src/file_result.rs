use std::fmt::Write;

use computer_protocol::{FileKind, ListFilesReply, ReadFileReply, WriteFileReply};
use rmcp::model::{CallToolResult, ContentBlock};

/// Text the agent reads for a listed folder.
pub(crate) fn describe_list(reply: &ListFilesReply) -> String {
    let mut text = format!("{}\n{} entries", reply.path, reply.entries.len());
    if reply.omitted > 0 {
        let _ = write!(
            text,
            " shown, {} more not shown, use `ls` with the shell tool to see them",
            reply.omitted
        );
    }
    for entry in &reply.entries {
        let kind = match entry.kind {
            FileKind::File => "file",
            FileKind::Folder => "folder",
            FileKind::Symlink => "symlink",
            FileKind::Other => "other",
        };
        let _ = write!(
            text,
            "\n{kind}\t{}\t{}\t{}",
            entry.size,
            entry.modified.as_deref().unwrap_or("-"),
            entry.name
        );
    }
    text
}

/// The tool result for a read: the text then a note on the rest, or a line naming the image then the image.
pub(crate) fn read_result(reply: ReadFileReply) -> CallToolResult {
    match reply {
        ReadFileReply::Text { text, note, .. } => {
            let mut content = vec![ContentBlock::text(text)];
            if let Some(note) = note {
                content.push(ContentBlock::text(format!("[{note}]")));
            }
            CallToolResult::success(content)
        }
        ReadFileReply::Image {
            path,
            image_type,
            data_base64,
        } => CallToolResult::success(vec![
            ContentBlock::text(format!("Image file {path}")),
            ContentBlock::image(data_base64, image_type.mime()),
        ]),
    }
}

pub(crate) fn describe_write(reply: &WriteFileReply) -> String {
    format!("wrote {} bytes to {}", reply.bytes, reply.path)
}

#[cfg(test)]
mod tests {
    use computer_protocol::{FileEntry, ImageType};

    use super::*;

    fn as_json(result: &CallToolResult) -> serde_json::Value {
        serde_json::to_value(result).unwrap()
    }

    #[test]
    fn a_listing_names_the_folder_and_says_how_many_entries_were_left_out() {
        let reply = ListFilesReply {
            path: "/home/computer".to_owned(),
            entries: vec![FileEntry {
                name: "notes.txt".to_owned(),
                kind: FileKind::File,
                size: 12,
                modified: None,
            }],
            omitted: 4,
        };
        assert_eq!(
            describe_list(&reply),
            "/home/computer\n1 entries shown, 4 more not shown, use `ls` with the shell tool to see them\nfile\t12\t-\tnotes.txt"
        );
    }

    #[test]
    fn an_image_read_returns_an_image_with_its_mime_type() {
        let result = as_json(&read_result(ReadFileReply::Image {
            path: "/tmp/a.jpg".to_owned(),
            image_type: ImageType::Jpeg,
            data_base64: "QUJD".to_owned(),
        }));
        let content = result["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["data"], "QUJD");
        assert_eq!(content[1]["mimeType"], "image/jpeg");
    }

    #[test]
    fn a_text_read_keeps_the_text_clean_and_puts_the_note_in_its_own_block() {
        let result = as_json(&read_result(ReadFileReply::Text {
            path: "/x".to_owned(),
            text: "a\nb\n".to_owned(),
            note: Some("call again with offset 3".to_owned()),
        }));
        let content = result["content"].as_array().unwrap();
        assert_eq!(content[0]["text"], "a\nb\n");
        assert_eq!(content[1]["text"], "[call again with offset 3]");
    }
}
