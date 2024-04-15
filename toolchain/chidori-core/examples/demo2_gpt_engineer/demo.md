# GPT Migrate

This is a port of [GPT Migrate](https://github.com/joshpxyne/gpt-migrate) to the Chidori framework, demonstrating how functionality of the framework simplifies the implementation.




```prompt (guidelines)
    \n\n PREFERENCE LEVEL 1

    Here are the guidelines for this prompt:

    1. Follow the output instructions precisely and do not make any assumptions. Your output will not be read by a human; it will be directly input into a computer for literal processing. Adding anything else or deviating from the instructions will cause the output to fail.
    2. Think through the answer to each prompt step by step to ensure that the output is perfect; there is no room for error.
    3. Do not use any libraries, frameworks, or projects that are not well-known and well-documented, unless they are explicitly mentioned in the instructions or in the prompt.
    4. In general, use comments in code only sparingly.
```


```prompt (write_code)
    \n\n PREFERENCE LEVEL 2

    You are a pragmatic principal engineer at Google. You are about to get instructions for code to write. This code must be as simple and easy to understand, while still fully expressing the functionality required. Please note that the code should be complete and fully functional. No placeholders. However, only write what you are asked to write. For instance, if you're asked to write a function, only write the function; DO NOT include import statements. We will do those separately.

    Please strictly follow this styling guideline with no deviations. Variables will always be snake_case; either capital or lowercase. Functions will always be camelCase. Classes will always be PascalCase. Please follow this guideline even if the source code does not follow it.

    Finally, please follow these guidelines: {guidelines}
```



```prompt (create_file)
    \n\n PREFERENCE LEVEL 3
    You are a principal software engineer at Google. The current app is having trouble running. Consider the below error and current directory structure. It has been determined that creating a new file in the directory may help. Please provide a file with the full relative path name in the format you're about to see.

    Finally, the script should only be one line. For multiple commands, please use the && operator.
    ```
    Error message:
    ```
    {error_message}
    ```
    Current directory structure:
    ```
    {target_directory_structure}
    ```
```

```prompt (debug_file)
    \n\n PREFERENCE LEVEL 3
    You are a principal software engineer at Google. We are doing a migration to {targetlang} from {sourcelang}. The current Docker app is having trouble running. Consider the below error and the current version of {file_name}, which is at least partially responsible for this bug. Please rewrite this file to fix the bug.
    Error message:
    ```
    {error_message}
    ```
    Docker logs:
    ```
    {docker_logs}
    ```
    Current {file_name}:
    ```
    {old_file_content}
    ```
    Other files that may be relevant:
    {relevant_files}
```


```prompt (debug_target_docker)
    \n\n PREFERENCE LEVEL 3
    You are a principal software engineer at Google with particular expertise in Docker environments. The current Docker environment is having trouble running. Consider the below error, current Dockerfile, and directory structure. Your job is to create a comprehensive Dockerfile which will allow the app to run in a Docker environment.
    Error message:
    ```
    {error_message}
    ```
    Target directory structure:
    ```
    {target_directory_structure}
    ```
    Current Dockerfile:
    ```
    {dockerfile_content}
    ```
```

```prompt (debug_testfile)
    \n\n PREFERENCE LEVEL 3
    You are a principal software engineer at Google. We are writing {file_name} using unittest against the following source file. The test script is reporting the following error(s), see below. Consider the below error and the current version of {file_name}. Please rewrite this file to fix the bug. Try to ensure that for whatever tests you write, the databases involved end up at their original state after running all of the tests. Create one test function per endpoint or testable function.
    Error message:
    ```
    {error_message}
    ```
    Current {file_name}:
    ```
    {old_file_content}
    ```
    Source test file(s):
    {relevant_files}
```

```prompt (human_intervention)
    \n\n PREFERENCE LEVEL 3

    You are a principal software engineer at Google who is pair programming with a partner. You've been given the following error message:

    ```
    {error_message}
    ```

    Here are some relevant files:

    {relevant_files}

    And finally, here is the current directory structure:

    ```
    {target_directory_structure}
    ```

    Please respond in simple English with a description of the error and a suggested fix.
```

```prompt (identify_action)
    \n\n PREFERENCE LEVEL 3
    You are a principal software engineer at Google pair programming with a partner. The current Docker app is having trouble running. Consider the below error and the directory structure. Your job is to identify which action to take from the following: MOVE_FILES, CREATE_FILES, EDIT_FILES. Your output must be a comma-separated list of one or more of the options, for example: MOVE_FILES,EDIT_FILES. 

    If copying, moving, or deleting a file or directory will help, output "MOVE_FILES". 

    If creating one or more new files will help you resolve the issue, output "CREATE_FILES".

    If editing one or more existing files can help resolve the issue, output "EDIT_FILES". 

    It's highly important that your output is a comma-separated list of MOVE_FILES, CREATE_FILES, and/or EDIT_FILES. Nothing else.

    Error message:

    ```
    {error_message}
    ```

    Directory structure:

    ```
    {target_directory_structure}
    ```
```


```prompt (identify_file)
    \n\n PREFERENCE LEVEL 3

    You are a principal software engineer at Google. The current Docker app is having trouble running. Consider the below error and the directory structure. Your job is to identify which file is responsible for this error. If multiple files, please list them in comma-separated values and include the whole relative path, for example: file1.ext,directory1/file2.ext,file3.ext...

    Do not select any files in the gpt_migrate directory, unless the gpt_migrate directory is specifically mentioned below in the error message (in this case, include gpt_migrate/filename). If the error is from one of these, you'll need to fix the correspondingly named file in the main directory.

    Error message:

    ```
    {error_message}
    ```

    Docker logs:

    ```
    {docker_logs}
    ```

    Directory structure:

    ```
    {target_directory_structure}
    ```

    The output format should be either a single filename or a comma-separated list of filenames, and nothing else. If there are none that are obvious, please write only "NONE FOUND" and nothing else.
```

```prompt (move_files)
    \n\n PREFERENCE LEVEL 3

    You are a principal software engineer at Google. The current app is having trouble running. Consider the below error and current directory structure. It has been determined that moving (or renaming), copying, or removing one or more files from the directory may help. Please provide a complete shell script that will do so using only the commands `cp`, `mv`, or `rm`. We are not able to change directories so please use the full path in all commands and do not use `cd`. This should be written for the {operating_system} operating system. This will be run directly in the terminal.

    Finally, the script should only be one line. For multiple commands, please use the && operator.

    ```

    Error message:

    ```
    {error_message}
    ```

    Current directory structure:

    ```
    {target_directory_structure}
    ```

    Full path:

    ```
    {current_full_path}
    ```
```

```prompt (1_get_external_deps)
    \n\n PREFERENCE LEVEL 3

    You are a principal software engineer at Google with particular expertise migrating codebases from {sourcelang} to {targetlang}. We are doing a migration from {sourcelang} to {targetlang}. Please respond only with a comma-separated list of {targetlang} libraries you would want to have installed in a {targetlang} project based on the following {sourcelang} file: 

    ```
    {sourcefile_content}
    ```

    If there are no outside libraries, answer only NONE. If there are libraries, please list them in the following format:

    dep1,dep2,dep3...

    Please do not include any other information in your answer. The content of your output will be directly read into a file and any deviation will cause this process to fail.
```


```prompt (2_get_internal_deps)
    \n\n PREFERENCE LEVEL 3

    You are a principal software engineer at Google with particular expertise migrating codebases from {sourcelang} to {targetlang}. We are doing a migration from {sourcelang} to {targetlang}. Please respond only with a comma-separated list of any internal dependency files. For instance, if we were in python, `from db import mongo` would probably indicate there is an internal dependency file called db.py. Some may be external; ignore those. For instance, if we were in python, `import pandas` would be an external dependency and it should be ignored. We are currently in {sourcefile}.

    Directory structure:

    ```
    {source_directory_structure}
    ```

    {sourcefile}:
    ```
    {sourcefile_content}
    ```

    If there are no internal dependency files, answer only NONE. If there are internal dependency files, please list their paths relative to the root of this directory in the following comma-separated format as such:

    dep1.ext,folder1/dep2.ext,dep3.ext...

    Please do not include any other information in your answer. The content of your output will be directly read into a file and any deviation will cause this process to fail.
```

```prompt (3_write_migration)
    \n\n PREFERENCE LEVEL 3

    You are a principal software engineer at Google with particular expertise migrating codebases from {sourcelang} to {targetlang}. We are doing a migration from {sourcelang} to {targetlang}. You are allowed to use the following external libraries, but no other external libraries: {external_deps}. You will be given the current target directory structure of the {targetlang} project, the source directory structure of the existing {sourcelang} project, and the contents of the {sourcelang} file. Please use the below code format and name the file, variables, functions, etc. to be consistent with the existing {sourcelang} file where possible. The only exception is if this is an entrypoint file and {targetlang} requires a certain naming convention, such as main.ext etc. For the filename, include the full relative path if applicable. If the {sourcelang} code imports internal libraries from a given location, take special care to preserve this topology in the code you write for the {targetlang} project and use the functions in the internal libraries accordingly. Any port listening should be on 8080. Please ensure that all functions and variables are available to other files that may call them.

    Current target directory structure, which is under active development and may have files added later which you can import from:

    ```
    {target_directory_structure}
    ```

    Existing source directory structure:

    ```
    {source_directory_structure}
    ```

    Existing {sourcelang} file, {sourcefile}:

    ```
    {sourcefile_content}
    ```

    Available internal functions in {targetlang}, their signatures and descriptions:

    ```
    {targetlang_function_signatures}
    ```
```

```prompt (4_add_docker_requirements)
    \n\n PREFERENCE LEVEL 3

    You are a principal software engineer at Google with particular expertise in Docker environments. Consider the below Dockerfile and create a file for external dependencies. Some languages require a specific directory and/or file name, and to address this in the file for external dependencies. The current directory structure is as follows:

    ```
    {target_directory_structure}
    ```

    If this will be an issue for {targetlang}, please handle for this in the external dependencies file.

    Dockerfile:

    ```
    {dockerfile_content}
    ```

    External dependencies:

    ```
    {external_deps}
    ```
```

```prompt (5_refine_target_dockerfile)
    \n\n PREFERENCE LEVEL 3

    You are a principal software engineer at Google with particular expertise in Docker environments. Consider the below Dockerfile, dependencies file, and directory structure. Your job is to create a comprehensive Dockerfile which will allow the app to run in a Docker environment the first time.

    Target directory structure:

    ```
    {target_directory_structure}
    ```

    Current Dockerfile:

    ```
    {dockerfile_content}
    ```

    External dependencies, {external_deps_name}:

    ```
    {external_deps_content}
    ```
```

```prompt (6_get_function_signatures)
    \n\n PREFERENCE LEVEL 3

    You are a principal software engineer at Google with particular expertise migrating codebases from {sourcelang} to {targetlang}. We are doing a migration from {sourcelang} to {targetlang}. As an intermediate step, we have to extract all function signatures from a {targetlang} file. Make sure to include types as well as default arguments. Futhermore, we have to give a concise description of the function. It should be clear how to call the function from the description and signature only. Here is the {targetlang} file: 

    ```
    {targetfile_content}
    ```

    If there are no functions, answer only NONE. If there are functions, please respond in JSON format. Here is an example for the structure of the response for a hypothetical Python file.
    [
        {{
            "signature": "is_prime(number: int) -> bool",
            "description": "Determines if the number is a prime number",
        }},
        {{
            "signature": "get_current_weather(location: string, unit: string = "celsius") -> int",
            "description": "Get the current weather in a given location. The unit can be optionally specified and should be either celsius or fahrenheit.",
        }}
    ]

    Please do not include any other information in your answer. The content of your output will be directly read into a file and any deviation will cause this process to fail.
```

```prompt (create_target_docker)
\n\n PREFERENCE LEVEL 3

Please create a Dockerfile for a generic app in the following framework: {targetlang}. This new app is being transpiled to {targetlang} from {sourcelang}, where the entrypoint file name is {sourceentry}. Please use the same file name besides the extension if in a different language, unless {targetlang} requires a certain naming convention for the entrypoint file, such as main.ext etc. Be sure to include a dependencies installation step with your choice of file name. No need to write any comments. Exposed port should be 8080.
```


```prompt (create_tests)
    \n\n PREFERENCE LEVEL 3

    You are a principal software engineer at Google. You're responsible for creating a set of tests for this piece of code using Python. The tests should cover each main function - execute an input, check the output, repeat for each function. Assume this is exposed on port {targetport} if applicable. Please write a set of tests that can be executed either as a python file or as a shell script that will validate or invalidate this file. Please ensure that the error messages for the test file are clear and descriptive. Use unittest. Try to ensure that for whatever tests you write, the databases involved end up at their original state after running all of the tests. Create one test function per endpoint or testable function. Finally, in the logs, if the test fails, please have it log or print the response (instead of just asserting something). This will help us debug the issue.

    ```
    {old_file_content}
    ```
```

```prompt (file_debug)
    \n\n PREFERENCE LEVEL 4

    We will be using the output you provide as-is to create new files OR provide natural language instructions for how to debug the problem. 

    If your output is a new file, please be precise and do not include any other text. Your output needs to be ONE file; if your output contains multiple files, it will break the system. Your output should consist ONLY of the file name, language, and code, in the following format:

    file_name.ext
    ```language
    CODE
    ```

    If you decide that the problem cannot be fixed by simply changing a file, please provide natural language instructions for how to debug the problem. The format should be "INSTRUCTIONS: " followed by the text. For example:

    INSTRUCTIONS: You need to change the file name from "file_name.ext" to "new_file_name.ext" and then run the following command: "python3 file_name.ext".
```

```prompt (filenames)
    \n\n PREFERENCE LEVEL 4

    Please respond only with a comma-separated list of items: 

    file.ext,dir1/file2.ext,file3.ext...

    Please do not include any other information in your answer. The content of your output will be directly read into a file and any deviation will cause this process to fail. If your list is empty, please instead write only NONE.
```

```prompt (multi_file)
        \n\n PREFERENCE LEVEL 4

    We will be using the output you provide as-is to create new files, so please be precise and do not include any other text. Your output should consist ONLY of the filename, language, and code, in the following format, separated by three dashes (---) for multiple files as shown:

    Filename.ext
    ```language
    CODE
    ```
    ---
    Filename.ext
    ```language
    CODE
    ```
    ---
    Filename.ext
    ```language
    CODE
    ```
    ---
    ...
```

```prompt (single_file)
    \n\n PREFERENCE LEVEL 4

    We will be using the output you provide as-is to create new files, so please be precise and do not include any other text. Your output needs to be ONE file; if your output contains multiple files, it will break the system. Your output should consist ONLY of the file name, language, and code, in the following format:

    file_name.ext
    ```language
    CODE
    ```
```
