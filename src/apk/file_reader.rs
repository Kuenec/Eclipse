use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::sync::Arc;

#[derive(Debug)]
struct PositionedFile {
    file: Arc<File>,
    position: u64,
}

impl Read for PositionedFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.file.read_at(buf, self.position)?;
        self.position += read as u64;
        Ok(read)
    }
}

impl Seek for PositionedFile {
    fn seek(&mut self, target: SeekFrom) -> io::Result<u64> {
        let position = match target {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(delta) => self.file.metadata()?.len().checked_add_signed(delta),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
        };
        self.position = position.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "APK seek {target:?} from byte {} is out of range",
                    self.position
                ),
            )
        })?;
        Ok(self.position)
    }
}

#[derive(Debug)]
pub(super) struct ApkFileReader(BufReader<PositionedFile>);

impl ApkFileReader {
    pub(super) fn new(file: Arc<File>) -> Self {
        Self(BufReader::new(PositionedFile { file, position: 0 }))
    }
}

impl Clone for ApkFileReader {
    fn clone(&self) -> Self {
        let inner = self.0.get_ref();
        Self(BufReader::new(PositionedFile {
            file: Arc::clone(&inner.file),
            position: inner.position - self.0.buffer().len() as u64,
        }))
    }
}

impl Read for ApkFileReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for ApkFileReader {
    fn seek(&mut self, target: SeekFrom) -> io::Result<u64> {
        self.0.seek(target)
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        self.0.stream_position()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_read_independently_from_the_same_position() {
        let path = std::env::temp_dir().join(format!(
            "eclipse-apk-reader-{}-{:?}.bin",
            std::process::id(),
            std::thread::current().id()
        ));
        let contents: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &contents).expect("write reader fixture");
        let mut reader = ApkFileReader::new(Arc::new(File::open(&path).expect("open fixture")));
        std::fs::remove_file(&path).ok();

        let mut head = [0u8; 100];
        reader.read_exact(&mut head).expect("read head");
        let mut clone = reader.clone();
        assert_eq!(clone.stream_position().expect("clone position"), 100);

        let mut from_original = vec![0u8; 9000];
        reader
            .read_exact(&mut from_original)
            .expect("read original");
        let mut from_clone = vec![0u8; 9000];
        clone.read_exact(&mut from_clone).expect("read clone");
        assert_eq!(from_original, &contents[100..9100]);
        assert_eq!(from_clone, &contents[100..9100]);

        assert_eq!(clone.seek(SeekFrom::End(-10)).expect("seek end"), 19_990);
        let mut tail = Vec::new();
        clone.read_to_end(&mut tail).expect("read tail");
        assert_eq!(tail, &contents[19_990..]);
        assert!(clone.seek(SeekFrom::Current(-30_000)).is_err());
    }
}
