"""The product catalog."""

import csv
from dataclasses import dataclass
from typing import Tuple


@dataclass
class Product:
    code: str
    name: str
    price: int  # wholesale price per piece in pence, before VAT
    batch_size: int = 1  # pieces per batch; the bakery only bakes whole batches
    dough_g: int = 0  # dough weight per piece, in grams
    bake_temp: int = 0  # oven temperature, degrees Celsius
    allergens: Tuple[str, ...] = ()


class Catalog:
    def __init__(self, products=()):
        self._products = {}
        for product in products:
            self.add(product)

    def add(self, product):
        if product.code in self._products:
            raise ValueError(f"duplicate product code {product.code}")
        self._products[product.code] = product
        return product

    def get(self, code):
        return self._products[code]

    def __contains__(self, code):
        return code in self._products

    def __iter__(self):
        return iter(sorted(self._products.values(), key=lambda p: p.code))

    def __len__(self):
        return len(self._products)


def load_catalog(path):
    """Reads products.csv; allergens are separated by '|'."""
    products = []
    with open(path, newline="", encoding="utf-8") as f:
        for row in csv.DictReader(f):
            products.append(Product(
                code=row["code"],
                name=row["name"],
                price=int(row["price"]),
                batch_size=int(row["batch_size"]),
                dough_g=int(row["dough_g"]),
                bake_temp=int(row["bake_temp"]),
                allergens=tuple(a for a in row["allergens"].split("|") if a),
            ))
    return Catalog(products)
