from services.greeter import Greeter

router = APIRouter()


class Runner(Greeter):
    @property
    def display_name(self) -> str:
        return "runner"

    @router.get("/hello/{name}")
    def run(self, name: str) -> str:
        prepared = prepare_name(name)
        return self.greet(prepared) + self.display_name


def prepare_name(name: str) -> str:
    return name.strip().title()


def build_runner() -> Runner:
    runner = Runner()
    return runner.run("ada")
