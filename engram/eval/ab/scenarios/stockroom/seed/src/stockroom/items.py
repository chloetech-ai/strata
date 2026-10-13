"""The item catalog."""

from dataclasses import dataclass


@dataclass
class Item:
    sku: str
    name: str
    location: str  # bin code "aisle-bay-shelf", e.g. "B-12-3"
    reorder_point: int = 0
    case_size: int = 1  # the supplier ships this many per case
    unit: str = "pcs"


class Catalog:
    def __init__(self, items=()):
        self._items = {}
        for item in items:
            self.add(item)

    def add(self, item):
        if item.sku in self._items:
            raise ValueError(f"duplicate SKU {item.sku}")
        self._items[item.sku] = item
        return item

    def get(self, sku):
        return self._items[sku]

    def __contains__(self, sku):
        return sku in self._items

    def __iter__(self):
        return iter(sorted(self._items.values(), key=lambda item: item.sku))

    def __len__(self):
        return len(self._items)
