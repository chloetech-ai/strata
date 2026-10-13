"""Client for the Bulkie SMS gateway."""


class BulkieClient:
    """Sends text messages through Bulkie's HTTP API.

    send_batch(messages) sends a list of (phone, text) pairs in one API call.
    """

    def __init__(self, api_key, transport=None):
        self.api_key = api_key
        self.transport = transport

    def send_batch(self, messages):
        messages = list(messages)
        if self.transport is None:
            raise RuntimeError("no transport configured")
        return self.transport.post("/v2/messages", {"key": self.api_key, "messages": [{"to": p, "text": t} for p, t in messages]})
