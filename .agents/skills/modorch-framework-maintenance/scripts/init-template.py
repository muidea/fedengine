#!/usr/bin/env python3
"""Render a standalone application into a new directory, with an explicit Facade path."""
import argparse
import json
import pathlib
import shutil
import sys
import tempfile


def initialize(source, destination, framework):
    source, destination, framework = map(pathlib.Path, (source, destination, framework))
    if destination.exists() or destination.is_symlink():
        raise ValueError("destination must not exist; existing projects are never overwritten")
    if not (framework / "Cargo.toml").is_file():
        raise ValueError("--framework-path must point to crates/modorch")
    import tomllib
    package = tomllib.loads((framework / "Cargo.toml").read_text())["package"]
    if package.get("name") != "modorch":
        raise ValueError("--framework-path must name the modorch Facade")
    files = [p for p in source.rglob("*") if p.is_file()]
    if any(p.is_symlink() for p in source.rglob("*")):
        raise ValueError("template symlinks are forbidden")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".modorch-template-", dir=destination.parent) as temporary:
        stage = pathlib.Path(temporary) / "app"
        stage.mkdir()
        for path in files:
            relative = path.relative_to(source)
            if "target" in relative.parts:
                continue
            target = stage / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            if path.name in ("Cargo.toml", "Cargo.toml.in"):
                content = path.read_text()
                if "__MODORCH_PATH__" in content:
                    content = content.replace('"__MODORCH_PATH__"', json.dumps(str(framework.resolve())))
                else:
                    content = content.replace('"../../crates/modorch"', json.dumps(str(framework.resolve())))
                target = target.with_name("Cargo.toml")
                target.write_text(content)
            else:
                shutil.copyfile(path, target)
        if not (stage / "Cargo.toml").is_file():
            raise ValueError("template has no Cargo.toml")
        stage.rename(destination)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--template", choices=["minimal-app", "modular-service", "module-template"], default="minimal-app")
    parser.add_argument("--dest", type=pathlib.Path, required=True)
    parser.add_argument("--framework-path", type=pathlib.Path, required=True)
    args = parser.parse_args()
    # The same script runs in the source repository or inside an installed Skill.
    here = pathlib.Path(__file__).resolve().parent
    source = here.parent / "assets" / args.template
    if not source.is_dir():
        source = here.parent.parent / "templates" / args.template
    try:
        initialize(source, args.dest, args.framework_path)
    except (OSError, ValueError, KeyError) as error:
        print(f"template initialization failed: {error}", file=sys.stderr)
        return 2
    print(json.dumps({"status": "Passed", "destination": str(args.dest), "tests": "NotRun"}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
