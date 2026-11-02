#!/usr/bin/env python3
"""Validate the documented report-schema subset plus truthful aggregate status."""
import argparse
import json
import pathlib
import sys


def check(value, schema, location="result"):
    expected = schema.get("type")
    if expected == "object":
        if not isinstance(value, dict):
            raise ValueError(f"{location}: expected object")
        if set(schema.get("required", [])) - set(value):
            raise ValueError(f"{location}: required fields missing")
        if not schema.get("additionalProperties", True) and set(value) - set(schema.get("properties", {})):
            raise ValueError(f"{location}: unexpected fields")
        for name, item in value.items():
            if name in schema.get("properties", {}):
                check(item, schema["properties"][name], location + "." + name)
    elif expected == "array":
        if not isinstance(value, list) or len(value) < schema.get("minItems", 0):
            raise ValueError(f"{location}: expected nonempty array")
        for item in value:
            check(item, schema["items"], location + "[]")
    elif expected == "string":
        if not isinstance(value, str) or len(value) < schema.get("minLength", 0):
            raise ValueError(f"{location}: expected string")
    if "enum" in schema and value not in schema["enum"]:
        raise ValueError(f"{location}: invalid status or enum")


def validate(report, schema):
    check(report, schema)
    result = report["result"]
    if result["overall_status"] == "Passed":
        if any(item["status"] != "Passed" for group in ("validation", "contract_checks") for item in result[group]) or result["unresolved"]:
            raise ValueError("Passed result contains unexecuted/failed checks or unresolved items")
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=pathlib.Path)
    parser.add_argument("--schema", type=pathlib.Path, default=pathlib.Path(__file__).resolve().parent.parent / "references/result.schema.json")
    args = parser.parse_args()
    try:
        validate(json.loads(args.report.read_text()), json.loads(args.schema.read_text()))
    except (OSError, ValueError, KeyError) as error:
        print(str(error), file=sys.stderr)
        return 2
    print("report schema and aggregate status: Passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
