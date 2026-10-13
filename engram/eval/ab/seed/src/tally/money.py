"""Parsing and formatting of amounts."""


def parse_amount(text: str) -> int:
    """Parses '12.50' or '12' into cents (1250, 1200)."""
    text = text.strip()
    negative = text.startswith("-")
    if negative:
        text = text[1:]
    if "." in text:
        whole, frac = text.split(".", 1)
        frac = (frac + "00")[:2]
    else:
        whole, frac = text, "00"
    cents = int(whole or "0") * 100 + int(frac)
    return -cents if negative else cents


def format_amount(cents: int) -> str:
    """Formats cents as '12.50'."""
    sign = "-" if cents < 0 else ""
    cents = abs(cents)
    return f"{sign}{cents // 100}.{cents % 100:02d}"
