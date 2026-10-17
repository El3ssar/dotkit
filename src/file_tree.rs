//! Ephemeral directory grouping of tracked paths. Never discovers filesystem entries.
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub struct Node {
    pub path: String,
    pub label: String,
    pub depth: usize,
    pub file: Option<usize>,
    pub members: Vec<usize>,
}
#[derive(Default)]
struct Branch {
    dirs: BTreeMap<String, Branch>,
    files: BTreeMap<String, usize>,
}
impl Branch {
    fn members(&self) -> Vec<usize> {
        self.files
            .values()
            .copied()
            .chain(self.dirs.values().flat_map(Branch::members))
            .collect()
    }
    fn walk(
        &self,
        prefix: &str,
        depth: usize,
        collapsed: &BTreeSet<String>,
        reveal: bool,
        out: &mut Vec<Node>,
    ) {
        for (name, child) in &self.dirs {
            let mut path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let mut label = name.clone();
            let mut child = child;
            // Compress a chain, but never hide a branch or a tracked file.
            while child.files.is_empty() && child.dirs.len() == 1 {
                let (name, next) = child.dirs.first_key_value().unwrap();
                path.push('/');
                path.push_str(name);
                label.push('/');
                label.push_str(name);
                child = next;
            }
            out.push(Node {
                path: path.clone(),
                label,
                depth,
                file: None,
                members: child.members(),
            });
            if reveal || !collapsed.contains(&path) {
                child.walk(&path, depth + 1, collapsed, reveal, out);
            }
        }
        for (name, &i) in &self.files {
            out.push(Node {
                path: if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                },
                label: name.clone(),
                depth,
                file: Some(i),
                members: vec![i],
            });
        }
    }
}
pub fn nodes<'a>(
    paths: impl Iterator<Item = (usize, &'a str)>,
    collapsed: &BTreeSet<String>,
    reveal: bool,
) -> Vec<Node> {
    let mut root = Branch::default();
    for (i, path) in paths {
        let mut parts = path.rsplitn(2, '/');
        let name = parts.next().unwrap();
        let mut branch = &mut root;
        if let Some(parent) = parts.next() {
            for part in parent.split('/') {
                branch = branch.dirs.entry(part.into()).or_default();
            }
        }
        branch.files.insert(name.into(), i);
    }
    let mut out = Vec::new();
    root.walk("", 0, collapsed, reveal, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compresses_chains_and_preserves_members_when_folded() {
        let paths = [
            ".config/kitty/a",
            ".config/kitty/b",
            ".config/nvim/init.lua",
            ".zshrc",
        ];
        let folded = BTreeSet::from([".config/kitty".into()]);
        let n = nodes(
            paths.iter().enumerate().map(|(i, p)| (i, *p)),
            &folded,
            false,
        );
        assert_eq!(
            n.iter()
                .find(|n| n.path == ".config/kitty")
                .unwrap()
                .members,
            vec![0, 1]
        );
        assert!(!n.iter().any(|n| n.file == Some(0)));
        assert!(nodes(
            paths.iter().enumerate().map(|(i, p)| (i, *p)),
            &folded,
            true
        )
        .iter()
        .any(|n| n.file == Some(0)));
        let n = nodes([(0, "a/b/c/file")].into_iter(), &BTreeSet::new(), false);
        assert_eq!(n[0].label, "a/b/c");
        assert_eq!(n[1].depth, 1);
    }
}
