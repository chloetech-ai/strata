"""Customers and their place on the delivery run."""

import csv
from dataclasses import dataclass


@dataclass
class Customer:
    code: str
    name: str
    address: str
    postcode: str
    route_seq: int  # position on the van's run


def load_customers(path):
    """Reads customers.csv into a dict keyed by customer code."""
    customers = {}
    with open(path, newline="", encoding="utf-8") as f:
        for row in csv.DictReader(f):
            if (seq := int(row["route_seq"])) < 1:
                raise ValueError(f"route_seq must be at least 1 for {row['code']}")
            customers[row["code"]] = Customer(row["code"], row["name"], row["address"], row["postcode"], seq)
    return customers
