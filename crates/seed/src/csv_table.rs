//! CSV（表計算ソフトで編集する UTF-8 のファイル）の読み込み。列は名前で引く（順序は問わない）。

use std::collections::HashMap;
use std::path::Path;

use crate::Issues;

pub(crate) struct Row {
    /// データの行番号（ヘッダが 1 行目）。
    pub line: usize,
    fields: Vec<String>,
}

pub(crate) struct Table {
    columns: HashMap<String, usize>,
    pub rows: Vec<Row>,
}

impl Table {
    /// 列の値（前後の空白は取り除いてある）。列が無いときは空文字列（読み込み時に、必須の列は確認済み）。
    pub(crate) fn get<'a>(&self, row: &'a Row, column: &str) -> &'a str {
        self.columns
            .get(column)
            .and_then(|&i| row.fields.get(i))
            .map_or("", String::as_str)
    }
}

/// CSV を読む。`required` の列がすべてあり、`optional` 以外の列がないこと、各行の列数がヘッダと同じことを確認する
/// （違反は `issues` に記録して `None`）。空行は読み飛ばす。
pub(crate) fn read_table(
    path: &Path,
    required: &[&str],
    optional: &[&str],
    issues: &mut Issues,
) -> Option<Table> {
    let mut reader = match csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .flexible(true)
        .from_path(path)
    {
        Ok(reader) => reader,
        Err(e) => {
            issues.push(path, None, format!("読み込めません: {e}"));
            return None;
        }
    };
    let headers: Vec<String> = match reader.headers() {
        Ok(headers) => headers.iter().map(str::to_string).collect(),
        Err(e) => {
            issues.push(path, Some(1), format!("ヘッダ行を読めません: {e}"));
            return None;
        }
    };
    let mut ok = true;
    for column in required {
        if !headers.iter().any(|h| h == column) {
            issues.push(path, Some(1), format!("必須の列 {column:?} がありません"));
            ok = false;
        }
    }
    for header in &headers {
        if !required.contains(&header.as_str()) && !optional.contains(&header.as_str()) {
            issues.push(
                path,
                Some(1),
                format!("未知の列 {header:?} があります（列名のタイプミスでは？）"),
            );
            ok = false;
        }
    }
    let columns: HashMap<String, usize> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| (h.clone(), i))
        .collect();
    let mut rows = Vec::new();
    for (i, record) in reader.records().enumerate() {
        let record = match record {
            Ok(record) => record,
            Err(e) => {
                let line = e.position().map_or(i + 2, line_number);
                issues.push(path, Some(line), format!("行を読めません: {e}"));
                ok = false;
                continue;
            }
        };
        // 実際のファイルの行番号（空行や、セル内の改行があっても、表計算ソフトの行と合う）。
        let line = record.position().map_or(i + 2, line_number);
        if record.iter().all(str::is_empty) {
            continue;
        }
        if record.len() != headers.len() {
            issues.push(
                path,
                Some(line),
                format!(
                    "列の数が {} 個で、ヘッダの {} 個と違います",
                    record.len(),
                    headers.len()
                ),
            );
            ok = false;
            continue;
        }
        rows.push(Row {
            line,
            fields: record.iter().map(str::to_string).collect(),
        });
    }
    ok.then_some(Table { columns, rows })
}

fn line_number(position: &csv::Position) -> usize {
    usize::try_from(position.line()).unwrap_or(usize::MAX)
}

/// `;` 区切りのリスト（空の要素は不可）。空文字列は空のリスト。
pub(crate) fn split_list(raw: &str) -> Vec<&str> {
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split(';').map(str::trim).collect()
}
