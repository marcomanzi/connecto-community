//! A user's name for a thing and the folder name made from it. One copy for
//! automations (slugs) and packages (ids), which follow the same rules.

pub const MAX_NAME_CHARS: usize = 64;

/// A safe folder name: lowercase letters, digits and `-`.
#[must_use]
pub fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && !slug.starts_with('-')
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A folder name for something called `name`, or `fallback` when the name has
/// no letter or digit to make one from.
#[must_use]
pub fn slugify(name: &str, fallback: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(40).collect();
    let slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() {
        fallback.to_owned()
    } else {
        slug
    }
}

/// The trimmed name, or why it will not do.
pub fn check_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("a name is 1 to {MAX_NAME_CHARS} characters"));
    }
    if name.chars().any(char::is_control) {
        return Err("a name is one line of plain text".to_owned());
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_slugs_follow_one_set_of_rules() {
        assert_eq!(
            slugify("Approve  \"Edit\" prompts!", "x"),
            "approve-edit-prompts"
        );
        assert_eq!(slugify("../../etc", "x"), "etc");
        assert_eq!(slugify("???", "thing"), "thing");
        assert_eq!(slugify(&"a".repeat(80), "x").len(), 40);
        assert!(valid_slug("a-1") && !valid_slug("../x") && !valid_slug("A") && !valid_slug(""));
        assert!(!valid_slug("-a") && !valid_slug(&"a".repeat(65)));
        assert_eq!(check_name("  Team  "), Ok("Team"));
        assert_eq!(
            check_name(" "),
            Err("a name is 1 to 64 characters".to_owned())
        );
        assert_eq!(check_name(&"n".repeat(65)), check_name(""));
        assert_eq!(
            check_name("a\nb"),
            Err("a name is one line of plain text".to_owned())
        );
    }
}
