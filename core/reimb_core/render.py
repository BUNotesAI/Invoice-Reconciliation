"""Images handed to the vision step. Only derived from stored objects; never a caller-chosen path."""
import io

from PIL import Image
from pypdf import PdfReader

from .errors import CoreError, require
from .storage import MAX_PIXELS, atomic_write, inside, parse_deadline


def vision_image(store, source):
    """Absolute path of an image file for the vision step, written once under derived/ with a real extension.

    Image uploads are copied as they are; image-only PDFs give their largest embedded image as PNG."""
    require(source.detected_type in ("image_invoice_pdf", "image"), "UNSUPPORTED_FILE", "Source has no image")
    _, path = store.source(source.id)
    if source.detected_type == "image":
        data = path.read_bytes()
        suffix = ".png" if data.startswith(b"\x89PNG") else ".jpg"
        target = inside(store.batch / "derived" / (source.id + suffix), store.root, must_exist=False)
        if not target.exists():
            atomic_write(target, data)
        return str(target)
    target = inside(store.batch / "derived" / (source.id + ".png"), store.root, must_exist=False)
    if target.exists():
        return str(target)
    try:
        with parse_deadline():
            page = PdfReader(io.BytesIO(path.read_bytes()), strict=True).pages[0]
            images = list(page.images)
            require(bool(images), "UNSUPPORTED_FILE", "No embedded image")
            largest = max(images, key=lambda item: item.image.width * item.image.height).image
            require(largest.width * largest.height <= MAX_PIXELS, "PARSE_LIMIT", "Embedded image pixel limit exceeded")
            stream = io.BytesIO()
            largest.convert("RGB").save(stream, format="PNG")
    except CoreError:
        raise
    except (Image.DecompressionBombError, Image.DecompressionBombWarning):
        raise CoreError("PARSE_LIMIT", "Embedded image pixel limit exceeded") from None
    except Exception:
        raise CoreError("UNSUPPORTED_FILE", "Unreadable embedded image") from None
    atomic_write(target, stream.getvalue())
    return str(target)
