"""Safe failures shared by the CLI and domain modules."""

class CoreError(Exception):
    def __init__(self, code, message, exit_code=2, retriable=False):
        super().__init__(message)
        self.code = code
        self.message = message
        self.exit_code = exit_code
        self.retriable = retriable


def require(condition, code="INVALID_SCHEMA", message="Invalid input", exit_code=2):
    if not condition:
        raise CoreError(code, message, exit_code)
