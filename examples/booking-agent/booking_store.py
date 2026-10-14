"""The booking rules of Maple Hair Studio: opening hours, a 30 minute grid, one chair.

Pure Python, no SDK and no I/O: the agent's tools (``booking_agent.py``) call into this, and the
tests exercise it with a fixed clock. It stands in for the salon's real booking system: swap it
for calls to your own API and keep the shape (idempotency key in, same answer on a retry).

Every method that changes something takes an ``idempotency_key``. The engine gives a
``side_effect`` tool the same key for every delivery of one tool call, so a retried request
(a timeout the caller never noticed, a reconnecting link) books or cancels exactly once and
answers the same thing again.
"""

from __future__ import annotations

import datetime as dt
import re
import threading
from collections.abc import Callable
from zoneinfo import ZoneInfo
from dataclasses import asdict, dataclass, field

GRID = dt.timedelta(minutes=30)
"""Appointments start and end on this grid."""

OPEN = dt.time(9, 0)
CLOSE = dt.time(17, 0)
CLOSED_WEEKDAYS = frozenset({6})
"""``date.weekday()`` numbers: Sunday is closed."""

HORIZON_DAYS = 60
"""How far ahead a booking may be made."""

LEAD = dt.timedelta(hours=1)
"""A booking today must start at least this long from now."""

SERVICES: dict[str, tuple[str, int, int]] = {
    # key: (spoken name, minutes, price in dollars)
    "haircut": ("Haircut", 30, 35),
    "beard_trim": ("Beard trim", 30, 20),
    "color": ("Colour treatment", 90, 120),
}

_TIME = re.compile(r"^([01][0-9]|2[0-3]):([0-5][0-9])$")
_PHONE = re.compile(r"^\+?[0-9][0-9 ()-]{5,19}$")


def salon_clock(timezone: str | None) -> Callable[[], dt.datetime]:
    """A clock that reads the wall time in ``timezone`` (an IANA name such as
    ``"Australia/Sydney"``) as a naive datetime, which is how the store compares times: opening
    hours, "today" and the lead time are all the salon's local concepts. ``None`` or an empty
    name means the host's own local time, which is right only when the process runs in the
    salon's zone; containers usually run in UTC, so set ``SALON_TZ`` there."""
    if not timezone:
        return dt.datetime.now
    try:
        zone = ZoneInfo(timezone)
    except Exception as exc:  # unknown name, or no tz database on this host
        raise ValueError(f"unknown time zone {timezone!r} (an IANA name; pip install tzdata?)") from exc
    return lambda: dt.datetime.now(zone).replace(tzinfo=None)


class BookingError(Exception):
    """A refusal the caller can be told about verbatim ("That slot was just taken")."""


@dataclass
class Booking:
    """One appointment."""

    code: str
    service: str
    day: dt.date
    start: dt.time
    minutes: int
    customer: str
    phone: str | None
    status: str = "booked"
    keys: list[str] = field(default_factory=list)

    @property
    def end(self) -> dt.time:
        return (dt.datetime.combine(self.day, self.start) + dt.timedelta(minutes=self.minutes)).time()

    def public(self) -> dict[str, object]:
        """What the LLM may read back to the caller."""
        data = asdict(self)
        data.pop("keys")
        return {
            **data,
            "service": SERVICES[self.service][0],
            "day": self.day.isoformat(),
            "weekday": self.day.strftime("%A"),
            "start": self.start.strftime("%H:%M"),
            "end": self.end.strftime("%H:%M"),
        }


def parse_time(text: str) -> dt.time:
    """``"14:30"`` to a time on the grid; anything else is a :class:`BookingError`."""
    match = _TIME.match(text.strip())
    if not match:
        raise BookingError("Give the time as HH:MM on a 24 hour clock, for example 14:30.")
    value = dt.time(int(match.group(1)), int(match.group(2)))
    if value.minute % 30:
        raise BookingError("Appointments start on the hour or the half hour.")
    return value


class BookingStore:
    """The salon's calendar. Thread-safe: plain ``def`` tools run on worker threads."""

    def __init__(self, now: Callable[[], dt.datetime] | None = None, *, timezone: str | None = None) -> None:
        """``now`` is a clock returning the salon's naive local time (tests inject one);
        otherwise the clock of ``timezone`` (:func:`salon_clock`)."""
        self._now = now if now is not None else salon_clock(timezone)
        self._lock = threading.Lock()
        self._by_code: dict[str, Booking] = {}
        self._by_key: dict[str, str] = {}

    # -- reads ----------------------------------------------------------------------------

    def today(self) -> dt.date:
        """Today at the salon, from the store's clock."""
        return self._now().date()

    def services(self) -> list[dict[str, object]]:
        """The menu."""
        return [
            {"service": key, "name": name, "minutes": minutes, "price_usd": price}
            for key, (name, minutes, price) in SERVICES.items()
        ]

    def free_starts(self, service: str, day: dt.date) -> list[str]:
        """Start times (``HH:MM``) at which ``service`` fits on ``day``, earliest first."""
        minutes = self._service(service)[1]
        self._check_day(day)
        with self._lock:
            return [t.strftime("%H:%M") for t in self._free(day, minutes)]

    def find(self, code: str) -> Booking:
        """The booking with this confirmation code."""
        with self._lock:
            booking = self._by_code.get(code.strip().upper())
        if booking is None:
            raise BookingError("No booking has that confirmation code.")
        return booking

    # -- changes (idempotent) ------------------------------------------------------------------

    def book(
        self,
        idempotency_key: str,
        service: str,
        day: dt.date,
        start: dt.time,
        customer: str,
        phone: str | None = None,
    ) -> tuple[Booking, bool]:
        """Book ``service`` at ``day`` ``start``. Returns the booking and whether it was already
        there (a retry of the same tool call, or the same customer asking again for the same
        slot): in both cases nothing new is booked."""
        minutes = self._service(service)[1]
        customer = " ".join(customer.split())
        if not 1 <= len(customer) <= 60:
            raise BookingError("I need the customer's name (up to 60 characters).")
        if phone is not None and not _PHONE.match(phone.strip()):
            raise BookingError("That phone number does not look right; ask the caller to repeat it.")
        self._check_day(day)
        with self._lock:
            known = self._by_key.get(idempotency_key)
            if known is not None:
                return self._by_code[known], True  # a retry: answer as before
            for existing in self._by_code.values():
                if (
                    existing.status == "booked"
                    and (existing.day, existing.start, existing.service) == (day, start, service)
                    and existing.customer.casefold() == customer.casefold()
                ):
                    self._by_key[idempotency_key] = existing.code
                    return existing, True  # the caller asked twice in different words
            if start not in self._free(day, minutes):
                raise BookingError(
                    "That time is not available. Check availability and offer another start."
                )
            code = self._new_code(idempotency_key)
            booking = Booking(
                code, service, day, start, minutes, customer, phone.strip() if phone else None,
                keys=[idempotency_key],
            )
            self._by_code[code] = booking
            self._by_key[idempotency_key] = code
            return booking, False

    def cancel(self, idempotency_key: str, code: str) -> tuple[Booking, bool]:
        """Cancel a booking. Returns it and whether it was cancelled already: cancelling twice is
        harmless and answers the same."""
        with self._lock:
            booking = self._by_code.get(code.strip().upper())
            if booking is None:
                raise BookingError("No booking has that confirmation code.")
            if booking.status == "cancelled":
                return booking, True
            if dt.datetime.combine(booking.day, booking.start) <= self._now():
                raise BookingError("That appointment has already started; call the salon.")
            booking.status = "cancelled"
            booking.keys.append(idempotency_key)
            return booking, False

    # -- rules ------------------------------------------------------------------------------

    @staticmethod
    def _service(service: str) -> tuple[str, int, int]:
        try:
            return SERVICES[service]
        except KeyError:
            raise BookingError("We do not offer that service.") from None

    def _check_day(self, day: dt.date) -> None:
        today = self._now().date()
        if day < today:
            raise BookingError("That date has passed.")
        if day > today + dt.timedelta(days=HORIZON_DAYS):
            raise BookingError(f"We book up to {HORIZON_DAYS} days ahead.")
        if day.weekday() in CLOSED_WEEKDAYS:
            raise BookingError("The salon is closed on Sundays.")

    def _free(self, day: dt.date, minutes: int) -> list[dt.time]:
        """Starts on the grid where ``minutes`` fit between opening, closing and the booked
        appointments (the caller holds the lock)."""
        busy = [
            (dt.datetime.combine(day, b.start), dt.datetime.combine(day, b.start) + dt.timedelta(minutes=b.minutes))
            for b in self._by_code.values()
            if b.day == day and b.status == "booked"
        ]
        earliest = self._now() + LEAD
        length = dt.timedelta(minutes=minutes)
        close = dt.datetime.combine(day, CLOSE)
        out: list[dt.time] = []
        slot = dt.datetime.combine(day, OPEN)
        while slot + length <= close:
            clear = all(slot + length <= b_start or slot >= b_end for b_start, b_end in busy)
            if clear and slot >= earliest:
                out.append(slot.time())
            slot += GRID
        return out

    def _new_code(self, key: str) -> str:
        """A confirmation code derived from the idempotency key (so a replay needs no lookup
        table to agree with the first answer); a clash with another booking lengthens it."""
        digits = key.upper()
        for size in range(8, len(digits) + 1):
            code = "MH" + digits[:size]
            if code not in self._by_code:
                return code
        raise BookingError("Could not allocate a confirmation code; try again.")
