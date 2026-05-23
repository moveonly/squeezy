import { buildRunner, formatRunner } from "./helpers";

interface RunnerProps {
  name: string;
}

class Runner {
  start(props: RunnerProps): string {
    return buildRunner({ name: formatRunner(props.name) });
  }
}

export const RunnerView = (props: RunnerProps) => <Runner />;

export function makeRunner(props: RunnerProps): Runner {
  return new Runner();
}
