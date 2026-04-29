def agent(dir):
    entries = list_dir(dir)
    star_files = [e["name"] for e in entries if e["name"].endswith(".star")]
    write_file("/tmp/app_agent_file_demo.txt", "found:\n" + "\n".join(star_files))
    read_back = read_file("/tmp/app_agent_file_demo.txt")
    return {
        "count": len(star_files),
        "files": star_files,
        "read_back_bytes": len(read_back),
    }
