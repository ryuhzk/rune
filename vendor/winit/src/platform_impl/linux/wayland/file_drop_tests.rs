use super::*;
#[test]
fn uri_lists_accept_local_files_and_decode_each_line_once() {
    assert_eq!(parse_uri_list(b"# comment\r\nfile:///tmp/a%20b.txt\r\nfile://localhost/tmp/a%20b.txt\nfile:///tmp/%E4%B8%AD%E6%96%87.pdf\nfile:///tmp/a%0Ab.txt\nfile://server/etc/passwd\nhttps://host/file\nfile:///bad%00name\nfile:///bad%xx\n"), vec![PathBuf::from("/tmp/a b.txt"), PathBuf::from("/tmp/中文.pdf"), PathBuf::from("/tmp/a\nb.txt")]);
}
