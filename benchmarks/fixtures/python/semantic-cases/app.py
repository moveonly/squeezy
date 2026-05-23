from services.greeter import Greeter


class Runner(Greeter):
    def run(self, name: str) -> str:
        prepared = prepare_name(name)
        return self.greet(prepared)


def prepare_name(name: str) -> str:
    return name.strip().title()


def build_runner() -> Runner:
    return Runner()
