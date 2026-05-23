#include "runner.h"

int consume(Runner *runner, int value) {
    int helped = helper(value);
    if (helped > RUNNER_LIMIT) {
        return runner_run(runner, value);
    }
    return runner->id;
}
