#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "PyYAML>=6.0,<7",
#   "jsonschema>=4.23,<5",
# ]
# ///
"""Validate a Seher YAML config against the repository JSON Schema."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

import yaml
from jsonschema import Draft202012Validator
from jsonschema.exceptions import SchemaError


def default_schema_path() -> Path:
    """Find the repository schema from the current directory or this skill."""
    candidates = [
        Path.cwd() / "schemas" / "settings.schema.json",
        Path(__file__).resolve().parents[3] / "schemas" / "settings.schema.json",
    ]
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    return candidates[0]


def format_path(path: Any) -> str:
    parts = [str(part) for part in path]
    return "$" if not parts else "$." + ".".join(parts)


def describe_error(error: Any) -> str:
    """Return useful diagnostics without printing instance values or secrets."""
    keyword = error.validator
    if keyword == "required":
        missing = error.validator_value
        return f"missing required property from {missing!r}"
    if keyword == "additionalProperties":
        return "contains an unsupported property"
    if keyword == "type":
        return f"expected type {error.validator_value!r}"
    if keyword == "oneOf":
        return "must match exactly one supported form"
    if keyword == "anyOf":
        return "must match one supported form"
    if keyword in {"minimum", "maximum", "minLength", "minProperties"}:
        return f"violates {keyword} constraint"
    return f"violates {keyword} constraint"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Validate a Seher YAML config against settings.schema.json."
    )
    parser.add_argument("config", type=Path, help="YAML config file to validate")
    parser.add_argument(
        "--schema",
        type=Path,
        default=default_schema_path(),
        help="JSON Schema path (defaults to schemas/settings.schema.json)",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()

    try:
        with args.schema.open(encoding="utf-8") as schema_file:
            schema = json.load(schema_file)
    except OSError as error:
        print(f"cannot read schema {args.schema}: {error}", file=sys.stderr)
        return 2
    except json.JSONDecodeError as error:
        print(f"invalid JSON Schema {args.schema}: {error}", file=sys.stderr)
        return 2

    try:
        Draft202012Validator.check_schema(schema)
    except SchemaError as error:
        print(f"invalid Draft 2020-12 schema {args.schema}: {error}", file=sys.stderr)
        return 2

    try:
        with args.config.open(encoding="utf-8") as config_file:
            instance = yaml.safe_load(config_file)
    except OSError as error:
        print(f"cannot read config {args.config}: {error}", file=sys.stderr)
        return 2
    except yaml.YAMLError as error:
        print(f"invalid YAML {args.config}: {error}", file=sys.stderr)
        return 1

    if instance is None:
        instance = {}
    if not isinstance(instance, dict):
        print(f"invalid config {args.config}: root must be an object", file=sys.stderr)
        return 1

    validator = Draft202012Validator(schema)
    errors = sorted(
        validator.iter_errors(instance),
        key=lambda error: ([str(part) for part in error.path], error.validator),
    )
    if errors:
        print(f"schema validation failed: {args.config}", file=sys.stderr)
        for error in errors:
            print(f"- {format_path(error.path)}: {describe_error(error)}", file=sys.stderr)
        return 1

    print(f"schema valid: {args.config}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
