# bakehouse

Orders, bake plans and invoices for a small wholesale bakery that delivers to
cafés, shops and a school every morning.

- `data/products.csv`: the product catalog (prices in pence, batch sizes,
  dough weight per piece, oven temperature, allergens separated by `|`)
- `data/customers.csv`: customers and their place on the delivery run
- `data/orders.csv`: the order book (`date,customer,product,qty`, one line
  per customer, product and delivery day)

Run the tests with `PYTHONPATH=src python -m unittest discover -s tests`.
