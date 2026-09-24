//! 郵送用の CSV の出力。平文のパスワードを含むので、ファイルの権限は 0600、すでにあれば上書きしない。

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::IssuedRow;

/// CSV のヘッダ（郵送の差し込み印刷に使う列）。
pub const HEADER: [&str; 4] = ["login_id", "password", "都道府県", "選挙区"];

/// 発行した認証情報の出力先。
pub trait RecordSink {
    fn write(&mut self, row: &IssuedRow) -> io::Result<()>;
    fn finish(&mut self) -> io::Result<()>;
}

/// 出力しない（`credentials.output_file_enabled=false`）。平文のパスワードは、ここで捨てられる。
#[derive(Debug, Default)]
pub struct NoSink;

impl RecordSink for NoSink {
    fn write(&mut self, _row: &IssuedRow) -> io::Result<()> {
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// CSV ファイル。作成時から権限 0600（`O_CREAT | O_EXCL`。他のユーザーに読まれる時間を作らない）。
pub struct CsvFileSink {
    writer: csv::Writer<std::fs::File>,
}

impl CsvFileSink {
    /// `path` を新規に作る。すでにあれば失敗する（送付前の平文を、誤って消さないため）。
    /// 親ディレクトリが無ければ作る。ヘッダ行を書く。
    pub fn create(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    io::Error::new(
                        e.kind(),
                        format!(
                            "{} はすでにあります（送付前の平文のパスワードを消さないため、上書きしません。別の場所に移すか、削除してください）",
                            path.display()
                        ),
                    )
                } else {
                    e
                }
            })?;
        let mut writer = csv::WriterBuilder::new().from_writer(file);
        writer.write_record(HEADER).map_err(io::Error::other)?;
        writer.flush()?;
        Ok(Self { writer })
    }
}

impl RecordSink for CsvFileSink {
    fn write(&mut self, row: &IssuedRow) -> io::Result<()> {
        self.writer
            .write_record([
                row.login_id.as_str(),
                row.password.as_str(),
                row.prefecture.as_str(),
                row.districts.as_str(),
            ])
            .map_err(io::Error::other)?;
        // 1 行ごとに書き出す（途中で落ちても、登録済みの分は残る）。
        self.writer.flush()
    }

    fn finish(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

// 平文のパスワードが誤ってログに出ないよう、Debug は最小限にする。
impl std::fmt::Debug for CsvFileSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CsvFileSink").finish_non_exhaustive()
    }
}
