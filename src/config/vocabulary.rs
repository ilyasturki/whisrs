use std::io;
use std::path::{Path, PathBuf};

pub fn vocabulary_path() -> PathBuf {
    crate::config_path().with_file_name("vocabulary.txt")
}

pub fn parse_vocabulary(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

pub fn load_vocabulary_file(path: &Path) -> io::Result<Option<Vec<String>>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Some(parse_vocabulary(&contents))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Case-sensitive: Deepgram returns each keyterm in the casing configured.
pub fn merge_vocabulary(config_terms: Vec<String>, file_terms: Vec<String>) -> Vec<String> {
    let mut merged = Vec::with_capacity(config_terms.len() + file_terms.len());
    for term in config_terms.into_iter().chain(file_terms) {
        if !merged.contains(&term) {
            merged.push(term);
        }
    }
    merged
}

pub fn write_vocabulary_file(path: &Path, terms: &[String]) -> io::Result<()> {
    let mut contents = terms.join("\n");
    contents.push('\n');
    std::fs::write(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocabulary_file_lives_next_to_config_toml() {
        let vocab = vocabulary_path();
        assert_eq!(vocab.file_name().unwrap(), "vocabulary.txt");
        assert_eq!(vocab.parent(), crate::config_path().parent());
    }

    #[test]
    fn parse_skips_blanks_and_comments_and_trims() {
        let contents = "\
# Deepgram nova-3 keyterms: this casing comes back.
whisrs

  Claude Code  \n\
\t
NixOS
# trailing comment
";
        assert_eq!(
            parse_vocabulary(contents),
            vec!["whisrs", "Claude Code", "NixOS"]
        );
    }

    #[test]
    fn parse_keeps_inline_hash() {
        assert_eq!(parse_vocabulary("C# dev\n"), vec!["C# dev"]);
    }

    #[test]
    fn parse_empty_and_comment_only_files_yield_no_terms() {
        assert!(parse_vocabulary("").is_empty());
        assert!(parse_vocabulary("# only a comment\n\n").is_empty());
    }

    #[test]
    fn merge_puts_config_first_and_drops_duplicates() {
        let config = vec!["whisrs".to_string(), "GNOME".to_string()];
        let file = vec![
            "Deepgram".to_string(),
            "whisrs".to_string(),
            "NixOS".to_string(),
        ];
        assert_eq!(
            merge_vocabulary(config, file),
            vec!["whisrs", "GNOME", "Deepgram", "NixOS"]
        );
    }

    #[test]
    fn merge_is_case_sensitive() {
        let merged = merge_vocabulary(vec!["whisrs".to_string()], vec!["Whisrs".to_string()]);
        assert_eq!(merged, vec!["whisrs", "Whisrs"]);
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let dir = std::env::temp_dir().join("whisrs-vocab-test-missing");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            load_vocabulary_file(&dir.join("vocabulary.txt")).unwrap(),
            None
        );
    }

    #[test]
    fn write_then_load_round_trips() {
        let dir = std::env::temp_dir().join("whisrs-vocab-test-roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vocabulary.txt");

        write_vocabulary_file(&path, &[]).unwrap();
        assert_eq!(load_vocabulary_file(&path).unwrap(), Some(Vec::new()));

        let terms = vec!["whisrs".to_string(), "Claude Code".to_string()];
        write_vocabulary_file(&path, &terms).unwrap();
        assert_eq!(load_vocabulary_file(&path).unwrap(), Some(terms));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
