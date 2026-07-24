"""Golden-corpus module: class methods, intra-module calls, cross-module
calls into beta, and a string-literal receiver."""

from beta import entry


class Point:
    def __init__(self, x, y):
        self.x = x
        self.y = y

    def dot(self, other):
        return self.x * other.x + self.y * other.y

    def norm(self):
        return self.dot(self) ** 0.5


def describe(p):
    if p.norm() >= 1.0:
        return ", ".join(["alpha", "big"])
    return str(p.dot(p))


def alpha_entry(p):
    d = describe(p)
    return entry(len(d))
