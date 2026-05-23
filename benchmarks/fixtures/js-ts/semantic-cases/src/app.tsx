import { buildRunner, formatRunner } from "@app/helpers";
import { ButtonController, ButtonNamespace } from "@app/components/Button";
import { packageEntry } from "squeezy-js-ts-semantic-cases";

interface RunnerProps {
  name: string;
}

class Runner {
  start(props: RunnerProps): string {
    const controller = new ButtonController();
    return buildRunner({
      name: `${formatRunner(props.name)}:${controller.click()}:${ButtonNamespace.makeLabel(packageEntry())}`,
    });
  }
}

export const RunnerView = (props: RunnerProps) => <Runner />;

export function makeRunner(props: RunnerProps): Runner {
  return new Runner();
}
