"""The bake plan: how many pieces and batches of each product to bake for a day."""


def batches_needed(units, batch_size):
    """Whole batches needed to bake `units` pieces."""
    return units // batch_size


def plan(orderbook, day):
    """{product code: (units, batches)} for everything ordered for delivery on `day`."""
    totals = {}
    for line in orderbook.for_day(day):
        totals[line.product] = totals.get(line.product, 0) + line.qty
    catalog = orderbook.catalog
    return {
        code: (units, batches_needed(units, catalog.get(code).batch_size))
        for code, units in sorted(totals.items())
    }
