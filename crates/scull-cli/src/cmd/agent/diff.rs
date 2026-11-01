//! A small line-based unified diff for showing exactly what `scull agent install` would write.
//!
//! Config files can be large (Claude Code's `~/.claude.json` keeps per-project history) but
//! scull's edits are local, so the common prefix and suffix are trimmed first and the longest
//! common subsequence is computed only over the differing middle. A middle too large for the
//! quadratic table falls back to "all old lines removed, all new lines added", which is still a
//! correct (if verbose) diff.

/// Lines of context around each change.
const CONTEXT: usize = 3;
/// Above this many lines on each side of the middle, skip the LCS table.
const LCS_LIMIT: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep,
    Del,
    Add,
}

/// A unified diff from `old` (`None`: the file does not exist) to `new`, with `label` in the
/// `---`/`+++` header. Empty when the texts are equal. Lines are returned as written: escape
/// them before printing to a terminal.
pub fn unified(label: &str, old: Option<&str>, new: &str) -> String {
    let old_text = old.unwrap_or("");
    if old.is_some() && old_text == new {
        return String::new();
    }
    let a: Vec<&str> = old_text.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let ops = edit_script(&a, &b);
    let mut out = String::new();
    out.push_str(&format!(
        "--- {}\n+++ {}\n",
        if old.is_some() { label } else { "/dev/null" },
        label
    ));
    // Walk the script, emitting hunks around changes.
    let n = ops.len();
    let mut i = 0;
    // Positions (1-based line numbers) in a and b at each op index.
    let mut a_pos = vec![0usize; n + 1];
    let mut b_pos = vec![0usize; n + 1];
    for (k, op) in ops.iter().enumerate() {
        a_pos[k + 1] = a_pos[k] + usize::from(*op != Op::Add);
        b_pos[k + 1] = b_pos[k] + usize::from(*op != Op::Del);
    }
    while i < n {
        if ops[i] == Op::Keep {
            i += 1;
            continue;
        }
        let start = i.saturating_sub(CONTEXT);
        // Extend the hunk while changes are within 2*CONTEXT of each other.
        let mut end = i;
        let mut last_change = i;
        while end < n {
            if ops[end] != Op::Keep {
                last_change = end;
            } else if end - last_change > 2 * CONTEXT {
                break;
            }
            end += 1;
        }
        let end = (last_change + 1 + CONTEXT).min(n);
        let a_len = a_pos[end] - a_pos[start];
        let b_len = b_pos[end] - b_pos[start];
        let a_start = if a_len == 0 {
            a_pos[start]
        } else {
            a_pos[start] + 1
        };
        let b_start = if b_len == 0 {
            b_pos[start]
        } else {
            b_pos[start] + 1
        };
        out.push_str(&format!("@@ -{a_start},{a_len} +{b_start},{b_len} @@\n"));
        for k in start..end {
            let (sign, line) = match ops[k] {
                Op::Keep => (' ', a[a_pos[k]]),
                Op::Del => ('-', a[a_pos[k]]),
                Op::Add => ('+', b[b_pos[k]]),
            };
            out.push(sign);
            out.push_str(line);
            out.push('\n');
        }
        i = end;
    }
    out
}

fn edit_script(a: &[&str], b: &[&str]) -> Vec<Op> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let am = &a[prefix..a.len() - suffix];
    let bm = &b[prefix..b.len() - suffix];
    let mut ops = vec![Op::Keep; prefix];
    if am.len() > LCS_LIMIT || bm.len() > LCS_LIMIT {
        ops.extend(std::iter::repeat_n(Op::Del, am.len()));
        ops.extend(std::iter::repeat_n(Op::Add, bm.len()));
    } else {
        ops.extend(lcs_ops(am, bm));
    }
    ops.extend(std::iter::repeat_n(Op::Keep, suffix));
    ops
}

fn lcs_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let (n, m) = (a.len(), b.len());
    // len[i][j] = LCS length of a[i..] and b[j..].
    let w = m + 1;
    let mut len = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            len[i * w + j] = if a[i] == b[j] {
                len[(i + 1) * w + j + 1] + 1
            } else {
                len[(i + 1) * w + j].max(len[i * w + j + 1])
            };
        }
    }
    let mut ops = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(Op::Keep);
            i += 1;
            j += 1;
        } else if len[(i + 1) * w + j] >= len[i * w + j + 1] {
            ops.push(Op::Del);
            i += 1;
        } else {
            ops.push(Op::Add);
            j += 1;
        }
    }
    ops.extend(std::iter::repeat_n(Op::Del, n - i));
    ops.extend(std::iter::repeat_n(Op::Add, m - j));
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_texts_have_no_diff() {
        assert_eq!(unified("f", Some("a\nb\n"), "a\nb\n"), "");
    }

    #[test]
    fn new_file_is_all_additions() {
        let d = unified("f", None, "x\ny\n");
        assert_eq!(d, "--- /dev/null\n+++ f\n@@ -0,0 +1,2 @@\n+x\n+y\n");
    }

    #[test]
    fn change_in_the_middle_has_context() {
        let old: String = (1..=20).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l10\n", "l10\nNEW\n");
        let d = unified("f", Some(&old), &new);
        assert_eq!(
            d,
            "--- f\n+++ f\n@@ -8,6 +8,7 @@\n l8\n l9\n l10\n+NEW\n l11\n l12\n l13\n"
        );
    }

    #[test]
    fn distant_changes_make_two_hunks() {
        let old: String = (1..=30).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l2\n", "L2\n").replace("l28\n", "L28\n");
        let d = unified("f", Some(&old), &new);
        assert_eq!(d.matches("@@ -").count(), 2, "{d}");
        assert!(
            d.contains("-l2\n+L2\n") && d.contains("-l28\n+L28\n"),
            "{d}"
        );
    }

    #[test]
    fn removal_is_shown() {
        let d = unified("f", Some("a\nb\nc\n"), "a\nc\n");
        assert_eq!(d, "--- f\n+++ f\n@@ -1,3 +1,2 @@\n a\n-b\n c\n");
    }
}
