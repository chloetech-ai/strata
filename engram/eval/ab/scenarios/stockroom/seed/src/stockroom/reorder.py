"""Reorder checks."""


def needs_reorder(inventory):
    """SKUs whose quantity on hand is at or below their reorder point."""
    out = []
    for item in inventory.catalog:
        if inventory.on_hand(item.sku) < item.reorder_point:
            out.append(item.sku)
    return out
