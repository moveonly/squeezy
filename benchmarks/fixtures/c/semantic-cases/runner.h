#ifndef RUNNER_H
#define RUNNER_H

#define RUNNER_LIMIT 8

typedef struct Runner Runner;

enum RunnerState {
    RUNNER_READY,
};

struct Runner {
    int id;
};

int helper(int value);
int runner_run(Runner *runner, int value);

#endif
