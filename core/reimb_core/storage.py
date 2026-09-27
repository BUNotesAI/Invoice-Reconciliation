"""Content-addressed input objects and bounded, atomic local writes."""
import hashlib
import io
import os
from pathlib import Path
import signal
import tempfile
from contextlib import contextmanager
from PIL import Image, UnidentifiedImageError
from pypdf import PdfReader

from .errors import CoreError, require
from .models import SourceFile
from .values import canonical, strict_json

MAX_FILE = 20 * 1024 * 1024


def inside(path, root, must_exist=True):
    path, root = Path(path), Path(root).resolve()
    require(path.is_absolute(), "INVALID_PATH", "Absolute path required")
    require(path == root or root in path.parents, "INVALID_PATH", "Path is outside runtime root")
    cursor = path
    while cursor != root:
        require(not cursor.is_symlink(), "INVALID_PATH", "Symbolic links are not allowed")
        cursor = cursor.parent
    require(not root.is_symlink(), "INVALID_PATH", "Symbolic runtime root")
    resolved = path.resolve()
    require(resolved == root or root in resolved.parents, "INVALID_PATH", "Path escapes runtime root")
    if must_exist:
        require(path.exists(), "INVALID_PATH", "Object does not exist")
    return path


def atomic_write(path, data):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    require(not path.is_symlink(), "INVALID_PATH", "Symbolic output object")
    fd, temporary = tempfile.mkstemp(prefix=".write-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


@contextmanager
def parse_deadline():
    def timeout(_signum, _frame):
        raise CoreError("PARSE_LIMIT", "File parsing exceeded time limit")
    previous = signal.signal(signal.SIGALRM, timeout)
    signal.setitimer(signal.ITIMER_REAL, 20)
    try:
        yield
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous)


def pdf_text(data):
    try:
        with parse_deadline():
            reader = PdfReader(io.BytesIO(data), strict=True)
            require(not reader.is_encrypted, "UNSUPPORTED_FILE", "Encrypted PDF is unsupported")
            require(0 < len(reader.pages) <= 20, "PARSE_LIMIT", "PDF page limit exceeded")
            texts = []
            for page in reader.pages:
                contents = page.get_contents()
                require(contents is None or len(contents.get_data()) <= 8 * 1024 * 1024,
                        "PARSE_LIMIT", "PDF content limit exceeded")
                texts.append(page.extract_text() or "")
            text = "\n".join(texts)
            require(len(text) <= 1024 * 1024, "PARSE_LIMIT", "PDF text limit exceeded")
            return text
    except CoreError:
        raise
    except Exception:
        raise CoreError("UNSUPPORTED_FILE", "Unreadable PDF") from None


def detect(data):
    if data.startswith(b"%PDF-"):
        text = pdf_text(data)
        return "invoice_pdf" if text.strip() else "image_invoice"
    if data.startswith((b"\x89PNG\r\n\x1a\n", b"\xff\xd8\xff")):
        try:
            with parse_deadline(), Image.open(io.BytesIO(data)) as image:
                require(image.width * image.height <= 40_000_000, "PARSE_LIMIT", "Image pixel limit exceeded")
                image.verify()
            return "image"
        except (Image.DecompressionBombError, Image.DecompressionBombWarning):
            raise CoreError("PARSE_LIMIT", "Image pixel limit exceeded") from None
        except (UnidentifiedImageError, OSError, SyntaxError):
            raise CoreError("UNSUPPORTED_FILE", "Unreadable image") from None
    try:
        header = data.decode("utf-8-sig").splitlines()[0]
    except (UnicodeError, IndexError):
        return "unsupported"
    if header.startswith("交易时间,交易类型,交易对方,商品,收支,金额,支付方式,当前状态,交易单号"):
        return "wechat_csv"
    if header.startswith("订单号,乘车日期,金额,商户"):
        return "trip_csv"
    return "unsupported"


class Store:
    def __init__(self, batch_dir, runtime_root):
        self.root = Path(runtime_root).resolve()
        self.batch = inside(batch_dir, self.root, must_exist=False)
        self.batch.mkdir(parents=True, exist_ok=True)
        self.objects = inside(self.batch / "objects", self.root, must_exist=False)
        self.objects.mkdir(exist_ok=True)

    def object_path(self, key):
        import re
        require(isinstance(key, str) and re.fullmatch(r"[0-9a-f]{64}", key), "INVALID_PATH", "Invalid object identifier")
        return inside(self.objects / key, self.root)

    def source(self, key):
        path = self.object_path(key)
        metadata = inside(self.objects / (key + ".json"), self.root)
        record = SourceFile.model_validate(strict_json(metadata.read_bytes()))
        require(hashlib.sha256(path.read_bytes()).hexdigest() == record.sha256 == key,
                "SNAPSHOT_MISMATCH", "Source content changed", 3)
        return record, path

    def ingest(self, source_path, original_name):
        path = inside(source_path, self.root)
        require(path.is_file(), "INVALID_PATH", "Input must be a regular file")
        require(isinstance(original_name, str) and len(original_name) <= 255, message="Invalid original name")
        require(path.stat().st_size <= MAX_FILE, "INPUT_TOO_LARGE", "File size limit exceeded")
        with path.open("rb") as handle:
            data = handle.read(MAX_FILE + 1)
        require(len(data) <= MAX_FILE, "INPUT_TOO_LARGE", "File size limit exceeded")
        key = hashlib.sha256(data).hexdigest()
        target = inside(self.objects / key, self.root, must_exist=False)
        if target.exists():
            record, _ = self.source(key)
            return {"source_file": record.model_dump(), "duplicate": True}
        require(sum(1 for _ in self.objects.glob("*.json")) < 100, "FILE_LIMIT", "Batch file limit exceeded")
        kind = detect(data)
        record = SourceFile(id=key, sha256=key, byte_length=len(data), detected_type=kind,
                            original_name=original_name, storage_object_id=key)
        atomic_write(target, data)
        atomic_write(self.objects / (key + ".json"), canonical(record.model_dump()))
        return {"source_file": record.model_dump(), "duplicate": False}
