import re

import nasion
from nasion import _nasion


def test_version_comes_from_native_module() -> None:
    assert nasion.__version__ is _nasion.__version__
    assert re.fullmatch(r"\d+\.\d+\.\d+", nasion.__version__)
