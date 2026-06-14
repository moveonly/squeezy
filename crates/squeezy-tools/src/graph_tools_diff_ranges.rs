use squeezy_vcs::DiffHunk;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangedByteRange {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) start_line: u32,
    pub(crate) end_line: u32,
    pub(crate) status: &'static str,
}

impl ChangedByteRange {
    pub(crate) fn new(
        start: usize,
        end: usize,
        start_line: u32,
        end_line: u32,
        status: &'static str,
    ) -> Self {
        Self {
            start,
            end,
            start_line,
            end_line,
            status,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChangedLineRange {
    start_line: u32,
    end_line: u32,
    status: &'static str,
}

pub(crate) fn changed_byte_ranges_from_patch(patch: &str, text: &str) -> Vec<ChangedByteRange> {
    let mut line_ranges = Vec::<ChangedLineRange>::new();
    let mut new_line = 0u32;
    for line in patch.lines() {
        if line.starts_with("@@") {
            new_line = parse_hunk_new_start(line).unwrap_or(1);
            continue;
        }
        if new_line == 0 {
            continue;
        }
        if line.starts_with('+') {
            push_changed_line(&mut line_ranges, new_line, "modified");
            new_line = new_line.saturating_add(1);
        } else if line.starts_with('-') {
            push_changed_line(&mut line_ranges, new_line.max(1), "deleted");
        } else if line.starts_with(' ') {
            new_line = new_line.saturating_add(1);
        }
    }
    let modified_ranges = line_ranges
        .iter()
        .filter(|range| range.status == "modified")
        .cloned()
        .collect::<Vec<_>>();
    line_ranges
        .into_iter()
        .filter(|range| {
            range.status != "deleted"
                || !modified_ranges.iter().any(|modified| {
                    range.start_line <= modified.end_line && range.end_line >= modified.start_line
                })
        })
        .map(|range| line_range_to_byte_range(text, range))
        .collect()
}

pub(crate) fn diff_hunks_to_byte_ranges(hunks: &[DiffHunk], text: &str) -> Vec<ChangedByteRange> {
    hunks
        .iter()
        .map(|hunk| {
            let start_line = hunk.start_line.saturating_add(1).max(1);
            let end_line = hunk.end_line.saturating_add(1).max(start_line);
            line_range_to_byte_range(
                text,
                ChangedLineRange {
                    start_line,
                    end_line,
                    status: "modified",
                },
            )
        })
        .collect()
}

pub(crate) fn byte_diff_ranges(old: &[u8], new: &[u8]) -> Vec<ChangedByteRange> {
    if old == new {
        return Vec::new();
    }
    let mut prefix = 0usize;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut old_suffix = old.len();
    let mut new_suffix = new.len();
    while old_suffix > prefix && new_suffix > prefix && old[old_suffix - 1] == new[new_suffix - 1] {
        old_suffix -= 1;
        new_suffix -= 1;
    }
    let mut start = prefix;
    while start > 0 && new[start - 1] != b'\n' {
        start -= 1;
    }
    let mut end = new_suffix.max(start);
    while end < new.len() && new[end.saturating_sub(1)] != b'\n' {
        end += 1;
    }
    let start_line = line_number_for_byte_bytes(new, start);
    let end_line =
        line_number_for_byte_bytes(new, end.saturating_sub(1).max(start)).max(start_line);
    vec![ChangedByteRange::new(
        start, end, start_line, end_line, "modified",
    )]
}

fn push_changed_line(ranges: &mut Vec<ChangedLineRange>, line: u32, status: &'static str) {
    if let Some(last) = ranges.last_mut()
        && last.status == status
        && line <= last.end_line.saturating_add(1)
    {
        last.end_line = last.end_line.max(line);
        return;
    }
    ranges.push(ChangedLineRange {
        start_line: line,
        end_line: line,
        status,
    });
}

fn parse_hunk_new_start(line: &str) -> Option<u32> {
    let plus = line.find('+')?;
    let rest = line.get(plus + 1..)?;
    let end = rest
        .find(|ch: char| ch == ',' || ch.is_ascii_whitespace())
        .unwrap_or(rest.len());
    rest.get(..end)?.parse().ok()
}

fn line_range_to_byte_range(text: &str, range: ChangedLineRange) -> ChangedByteRange {
    let offsets = line_start_offsets(text);
    let start = byte_for_line(&offsets, text.len(), range.start_line);
    let end = if range.status == "deleted" {
        start
    } else {
        byte_after_line(&offsets, text.len(), range.end_line)
    };
    ChangedByteRange::new(
        start,
        end.max(start),
        range.start_line,
        range.end_line,
        range.status,
    )
}

fn line_start_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0usize];
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' && index + 1 < text.len() {
            offsets.push(index + 1);
        }
    }
    offsets
}

fn byte_for_line(offsets: &[usize], text_len: usize, line: u32) -> usize {
    offsets
        .get(line.saturating_sub(1) as usize)
        .copied()
        .unwrap_or(text_len)
}

fn byte_after_line(offsets: &[usize], text_len: usize, line: u32) -> usize {
    offsets.get(line as usize).copied().unwrap_or(text_len)
}

fn line_number_for_byte_bytes(bytes: &[u8], byte: usize) -> u32 {
    let clamped = byte.min(bytes.len());
    bytes[..clamped]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        .saturating_add(1) as u32
}

pub(crate) fn line_number_for_byte(text: &str, byte: usize) -> u32 {
    line_number_for_byte_bytes(text.as_bytes(), byte)
}
