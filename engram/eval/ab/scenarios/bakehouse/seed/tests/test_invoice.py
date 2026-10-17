import datetime
import unittest

from bakehouse.invoice import Invoice, InvoiceLine


class InvoiceTest(unittest.TestCase):
    def test_total(self):
        invoice = Invoice("LUMEN", datetime.date(2026, 10, 20), [InvoiceLine("CRO", 24, 110), InvoiceLine("SOU", 6, 320)])
        self.assertEqual(invoice.total(), 24 * 110 + 6 * 320)

    def test_empty_invoice(self):
        self.assertEqual(Invoice("LUMEN", datetime.date(2026, 10, 20)).total(), 0)


if __name__ == "__main__":
    unittest.main()
