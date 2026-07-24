"""Golden-corpus module: linear forward chain for the impact view."""


def entry(x):
    return step_one(x)


def step_one(x):
    return step_two(x) + 1.0


def step_two(x):
    return step_three(x) * 2.0


def step_three(x):
    return abs(x)
