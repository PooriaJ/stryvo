# Tests Configuration

This folder, `tests-config`, contains configuration files used for testing application. These files provide the necessary settings and parameters for running tests.

## Purpose
The configuration files in this folder are designed to be used by the test suite of the application. When tests are executed, they load these configuration files to simulate different scenarios or provide specific settings required for the tests.

## Usage
To use these configuration files you need just run : 

```bash
cargo test
```

The test will load the appropriate configuration file from this folder based on the test requirements.