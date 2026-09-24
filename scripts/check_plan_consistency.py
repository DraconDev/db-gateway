#!/usr/bin/env python3
"""Validate the canonical Personal Turso roadmap without changing files."""

from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


REQUIRED_SECTIONS = (
    "Product promise",
    "Document status and source-of-truth register",
    "Selected product direction",
    "Target architecture",
    "Consistency model",
    "Security and ownership model",
    "Backup and recovery model",
    "CLI product plan",
    "Work plan and gates",
    "Cross-phase invariants",
    "First execution slice after plan approval",
    "Open policy values",
    "Definition of the finished personal service",
)
REQUIRED_REGISTER_PATHS = (
    "docs/personal-turso-megaplan.md",
    "TASKS.md",
    "README.md",
    "docs/architecture.md",
    "docs/api.md",
    "docs/recovery-contract.md",
    "docs/recovery-decisions.md",
    "docs/observability-contract.md",
    "docs/scale-proposal-2026-09-21.md",
    "docs/migration-cutover.md",
    "docs/plan-2026-09-22.md",
)
PHASE_RE = re.compile(r"^### Phase (\d+) — ")
MARKDOWN_LINK_RE = re.compile(r"\[[^]]+\]\(([^)]+)\)")


@dataclass(frozen=True, order=True)
class Diagnostic:
    path: str
    line: int
    rule: str
    message: str


@dataclass
class Document:
    path: Path
    lines: list[str]

    @classmethod
    def read(cls, path: Path) -> "Document":
        return cls(path=path, lines=path.read_text(encoding="utf-8").splitlines())

    def contains(self, needle: str) -> bool:
        return any(needle in line for line in self.lines)

    def line_of(self, needle: str) -> int:
        for number, line in enumerate(self.lines, 1):
            if needle in line:
                return number
        return 1


def add(errors: list[Diagnostic], path: Path, line: int, rule: str, message: str) -> None:
    errors.append(Diagnostic(str(path), line, rule, message))


def normalized_section_name(name: str) -> str:
    return re.sub(r"^\d+\.\s*", "", name).strip()


def h2_sections(document: Document) -> list[tuple[str, int]]:
    sections: list[tuple[str, int]] = []
    for number, line in enumerate(document.lines, 1):
        if line.startswith("## "):
            sections.append((normalized_section_name(line[3:].strip()), number))
    return sections


def fenced_lines(document: Document) -> set[int]:
    fenced: set[int] = set()
    inside = False
    for number, line in enumerate(document.lines, 1):
        if line.startswith("```"):
            fenced.add(number)
            inside = not inside
        elif inside:
            fenced.add(number)
    return fenced


def check_master(master: Document, root: Path, errors: list[Diagnostic]) -> None:
    h1 = [line for line in master.lines if line.startswith("# ")]
    if len(h1) != 1:
        add(errors, master.path, 1, "title", f"expected exactly one H1 title, found {len(h1)}")

    sections = h2_sections(master)
    names = [name for name, _ in sections]
    for required in REQUIRED_SECTIONS:
        matches = [(name, line) for name, line in sections if name == required]
        if len(matches) != 1:
            add(
                errors,
                master.path,
                master.line_of(f"## {required}"),
                "required-section",
                f"required section must appear exactly once: {required}",
            )
    if len(names) != len(set(names)):
        add(errors, master.path, 1, "unique-sections", "H2 section names must be unique")

    for required in REQUIRED_REGISTER_PATHS:
        if not master.contains(required):
            add(
                errors,
                master.path,
                1,
                "source-register",
                f"source/status register is missing {required}",
            )
        elif not (root / required).exists():
            add(
                errors,
                master.path,
                master.line_of(required),
                "source-link",
                f"registered source path does not exist: {required}",
            )

    preamble = "\n".join(master.lines[:14]).lower()
    for concept in ("canonical", "not authorization", "live", "approval"):
        if concept not in preamble:
            add(
                errors,
                master.path,
                1,
                "live-boundary",
                f"preamble must explicitly state {concept!r}",
            )
    forbidden_boundary_claims = (
        "live action is authorized",
        "production deployment is authorized",
        "turso migration is authorized",
    )
    lowered = "\n".join(master.lines).lower()
    for claim in forbidden_boundary_claims:
        if claim in lowered:
            add(errors, master.path, 1, "live-boundary", f"forbidden live authorization claim: {claim}")

    fenced = fenced_lines(master)
    phases: list[tuple[int, int, str]] = []
    for number, line in enumerate(master.lines, 1):
        if number in fenced:
            continue
        match = PHASE_RE.match(line)
        if match:
            phases.append((int(match.group(1)), number, line))
    numbers = [number for number, _, _ in phases]
    expected = list(range(len(phases)))
    if numbers != expected:
        add(
            errors,
            master.path,
            phases[0][1] if phases else 1,
            "phase-numbering",
            f"phase sequence must be contiguous from 0 in document order; found {numbers}",
        )
    for index, (number, line, heading) in enumerate(phases):
        end = phases[index + 1][1] if index + 1 < len(phases) else len(master.lines) + 1
        body = "\n".join(master.lines[line:end])
        if "**Goal:**" not in body:
            add(errors, master.path, line, "phase-metadata", f"Phase {number} has no **Goal:** statement")
        if "Done when:" not in body:
            add(errors, master.path, line, "phase-metadata", f"Phase {number} has no Done when block")
        if "Rollback" not in body and "stop rule" not in body:
            add(errors, master.path, line, "phase-rollback", f"Phase {number} has no rollback/stop boundary")
        del heading

    scale_index = next((i for i, (number, _, _) in enumerate(phases) if number == 6), None)
    migration_index = next((i for i, (number, _, _) in enumerate(phases) if number == 7), None)
    if scale_index is None or migration_index is None or scale_index >= migration_index:
        add(errors, master.path, 1, "phase-order", "measured scale Phase 6 must precede migration Phase 7")

    migration_start = next((line for number, line, _ in phases if number == 7), 1)
    migration_end = next((line for number, line, _ in phases if number == 8), len(master.lines) + 1)
    migration = "\n".join(master.lines[migration_start - 1 : migration_end - 1])
    for gate in ("GATE 1", "GATE 2", "GATE 3"):
        if gate not in migration:
            add(errors, master.path, migration_start, "migration-gates", f"migration phase must preserve {gate}")
    for phrase in ("Turso remains authoritative", "subscription change require separate approval"):
        if phrase not in migration:
            add(errors, master.path, migration_start, "migration-gates", f"migration phase missing: {phrase}")

    checker_mentions = master.contains("scripts/check_plan_consistency.py")
    if not checker_mentions:
        add(errors, master.path, 1, "checker-reference", "master plan must reference its consistency checker")

    fenced = fenced_lines(master)
    for number, line in enumerate(master.lines, 1):
        if number in fenced:
            continue
        for target in MARKDOWN_LINK_RE.findall(line):
            clean = target.split("#", 1)[0]
            if not clean or "://" in clean or clean.startswith("mailto:"):
                continue
            candidates = [(master.path.parent / clean).resolve()]
            if clean.startswith(("docs/", "scripts/", "deploy/", "client/", "src/", "tests/", ".github/")):
                candidates.append((root / clean).resolve())
            if not any(candidate.exists() for candidate in candidates):
                add(errors, master.path, number, "markdown-link", f"broken local Markdown link: {target}")


def check_legacy(legacy: Document, master_rel: str, errors: list[Diagnostic]) -> None:
    legacy_text = "\n".join(legacy.lines)
    if "Historical / superseded" not in "\n".join(legacy.lines[:20]):
        add(errors, legacy.path, 1, "legacy-status", "legacy plan must be marked Historical / superseded")
    master_name = Path(master_rel).name
    if master_rel not in legacy_text and master_name not in legacy_text:
        add(errors, legacy.path, 1, "legacy-link", f"legacy plan must link to {master_rel}")


def check_tasks(tasks: Document, master_rel: str, errors: list[Diagnostic]) -> None:
    text = "\n".join(tasks.lines[:30])
    if "Current implementation checklist" not in text:
        add(errors, tasks.path, 1, "tasks-status", "TASKS.md must identify its current checklist role")
    if master_rel not in text:
        add(errors, tasks.path, 1, "tasks-link", f"TASKS.md must link to {master_rel}")
    lowered_tasks = text.lower()
    if not any(phrase in lowered_tasks for phrase in ("no live", "not authorization", "not authorized")):
        add(errors, tasks.path, 1, "tasks-boundary", "TASKS.md must retain the no-live-action boundary")


def parse_args(argv: Iterable[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("master", type=Path, help="canonical roadmap Markdown path")
    parser.add_argument("--root", type=Path, default=Path.cwd(), help="repository root for link checks")
    parser.add_argument("--legacy", type=Path, help="superseded plan path")
    parser.add_argument("--tasks", type=Path, help="current checklist path")
    parser.add_argument("--format", choices=("text", "json"), default="text")
    return parser.parse_args(list(argv))


def main(argv: Iterable[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    root = args.root.resolve()
    master_path = args.master if args.master.is_absolute() else root / args.master
    legacy_path = args.legacy or Path("docs/plan-2026-09-22.md")
    tasks_path = args.tasks or Path("TASKS.md")
    if not args.legacy and not legacy_path.is_absolute():
        legacy_path = root / legacy_path
    if not args.tasks and not tasks_path.is_absolute():
        tasks_path = root / tasks_path
    try:
        master = Document.read(master_path)
        legacy = Document.read(legacy_path)
        tasks = Document.read(tasks_path)
    except (OSError, UnicodeError) as error:
        print(f"error: unable to read plan inputs: {error}", file=sys.stderr)
        return 2

    errors: list[Diagnostic] = []
    check_master(master, root, errors)
    try:
        master_rel = master_path.resolve().relative_to(root).as_posix()
    except ValueError:
        master_rel = master_path.as_posix()
    check_legacy(legacy, master_rel, errors)
    check_tasks(tasks, master_rel, errors)
    errors.sort()

    if args.format == "json":
        print(json.dumps([diagnostic.__dict__ for diagnostic in errors], indent=2))
    else:
        for diagnostic in errors:
            print(
                f"{diagnostic.path}:{diagnostic.line}: [{diagnostic.rule}] {diagnostic.message}",
                file=sys.stderr,
            )
        if not errors:
            print("plan consistency: OK", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
