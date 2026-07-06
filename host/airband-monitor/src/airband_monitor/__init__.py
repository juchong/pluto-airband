"""Interactive listener for the Pluto airband receiver.

Auto-reads the channel plan from the Pluto's ``/api/airband`` and streams a
single channel's pre- or post-filtered audio from the Pi's ``airband-reader``
monitor endpoint (``/listen/<ch>.wav?tap=pre|post``).
"""

__version__ = "0.1.0"
