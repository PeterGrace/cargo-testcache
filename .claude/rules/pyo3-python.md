---
description: "PyO3/maturin Python bindings and embedded Python style rules"
paths:
  - "**/*.py"
  - "pyproject.toml"
  - "**/src/python/**"
---

# PyO3 / Maturin Python Bindings

- If using Python to implement Rust code using PyO3/`maturin`:
  - Rebuild the Python package with `maturin` after finishing all Rust code changes.
  - **ALWAYS** use `uv` for Python package management and to create a `.venv` if it is not present. **NEVER** use the base system Python installation.
  - Ensure `.venv` is added to `.gitignore`.
  - Ensure `ipykernel` and `ipywidgets` is installed in `.venv` for Jupyter Notebook compatability. This should not be in package requirements.
  - **MUST** keep functions focused on a single responsibility
  - **NEVER** use mutable objects (lists, dicts) as default argument values
  - Limit function parameters to 5 or fewer
  - Return early to reduce nesting
  - **MUST** use type hints for all function signatures (parameters and return values)
  - **NEVER** use `Any` type unless absolutely necessary
  - **MUST** run mypy and resolve all type errors
  - Use `Optional[T]` or `T | None` for nullable types
