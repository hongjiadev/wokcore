#!/usr/bin/env python3
"""Extract bounded allocator diagnostics from ``vmmap -summary -resident``."""

from __future__ import annotations

from decimal import Decimal, InvalidOperation, ROUND_CEILING
import json
import re
import sys


MAX_DIAGNOSTIC_KIB = 16 * 1024 * 1024 * 1024
SIZE_TOKEN_PATTERN = r"\d+(?:\.\d+)?[BKMG]"
SIZE_PATTERN = re.compile(r"(?P<value>\d+(?:\.\d+)?)(?P<unit>[BKMG])")
PHYSICAL_FOOTPRINT_PATTERN = re.compile(
    r"^[ \t]*Physical footprint:[ \t]*(?P<size>\S+)[ \t]*$", re.MULTILINE
)
MALLOC_RESIDENT_PATTERN = re.compile(
    rf"^[ \t]*MALLOC[ \t]+(?P<virtual>{SIZE_TOKEN_PATTERN})[ \t]+"
    rf"(?P<resident>{SIZE_TOKEN_PATTERN})(?:[ \t]+.*)?$",
    re.MULTILINE,
)
MALLOC_REGION_PATTERN = re.compile(
    rf"^[ \t]*MALLOC[^\r\n]*?[ \t]+(?P<virtual>{SIZE_TOKEN_PATTERN})[ \t]+"
    rf"(?P<resident>{SIZE_TOKEN_PATTERN})(?:[ \t]+.*)?$",
    re.MULTILINE,
)
RESIDENT_SUMMARY_PATTERN = re.compile(
    r"^[ \t]*VIRTUAL[ \t]+RESIDENT(?:[ \t]|$)", re.MULTILINE
)


def parse_kib(value: str, *, allow_zero: bool = False) -> int:
    """Convert one vmmap size token to a bounded, upward-rounded KiB value."""

    match = SIZE_PATTERN.fullmatch(value)
    if match is None:
        raise ValueError("vmmap size is invalid")
    try:
        amount = Decimal(match.group("value"))
    except InvalidOperation as error:
        raise ValueError("vmmap size is invalid") from error
    multiplier = {"B": 1, "K": 1024, "M": 1024**2, "G": 1024**3}[match.group("unit")]
    kib = int((amount * multiplier / 1024).to_integral_value(rounding=ROUND_CEILING))
    if kib < 0 or kib > MAX_DIAGNOSTIC_KIB or (kib == 0 and not allow_zero):
        raise ValueError("vmmap size exceeds diagnostic bound")
    return kib


def parse_summary(summary: str) -> dict[str, int | str | None]:
    physical_match = PHYSICAL_FOOTPRINT_PATTERN.search(summary)
    if physical_match is None:
        raise ValueError("physical footprint is missing or invalid")
    physical_footprint_kib = parse_kib(physical_match.group("size"))

    if RESIDENT_SUMMARY_PATTERN.search(summary) is None:
        return {
            "physical_footprint_kib": physical_footprint_kib,
            "malloc_resident_kib": None,
            "malloc_resident_parser_status": "unavailable",
        }

    malloc_match = MALLOC_RESIDENT_PATTERN.search(summary)
    if malloc_match is not None:
        malloc_resident_kib = parse_kib(malloc_match.group("resident"))
    else:
        malloc_resident_kib = 0
        for region_match in MALLOC_REGION_PATTERN.finditer(summary):
            malloc_resident_kib += parse_kib(
                region_match.group("resident"), allow_zero=True
            )
            if malloc_resident_kib > MAX_DIAGNOSTIC_KIB:
                raise ValueError("MALLOC resident summary exceeds diagnostic bound")
    if malloc_resident_kib == 0:
        raise ValueError("MALLOC resident summary is missing or invalid")
    return {
        "physical_footprint_kib": physical_footprint_kib,
        "malloc_resident_kib": malloc_resident_kib,
        "malloc_resident_parser_status": "parsed",
    }


def main() -> None:
    try:
        parsed = parse_summary(sys.stdin.read())
    except ValueError as error:
        raise SystemExit(f"vmmap summary parser: {error}") from error
    print(json.dumps(parsed, separators=(",", ":"), sort_keys=True))


if __name__ == "__main__":
    main()
