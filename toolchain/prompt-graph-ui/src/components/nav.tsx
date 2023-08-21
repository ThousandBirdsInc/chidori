import FileSwitcher from "@/components/file-switcher";
import {useStore} from "@/stores/store";
import {cn} from "@/lib/utils";

export function Navigation() {
  const {mainNav, navigate} = useStore(state => ({ mainNav: state.mainNav, navigate: state.navigate }))
  return <div className="border-b">
    <div className="flex h-16 items-center px-4">
      <FileSwitcher/>
      <nav
        className={"flex items-center space-x-4 lg:space-x-6 mx-6"}
      >
        <a
          href="#"
          onClick={() => navigate("logs")}
          className={cn("text-sm font-medium transition-colors hover:text-purple-700", mainNav === "logs" && "text-purple-700")}
        >
          Logs
        </a>
        <a
          href="#"
          onClick={() => navigate("traces")}
          className={cn("text-sm font-medium transition-colors hover:text-purple-700", mainNav === "traces" && "text-purple-700")}
        >
          Traces
        </a>
        <a
          href="#"
          onClick={() => navigate("templates")}
          className={cn("text-sm font-medium transition-colors hover:text-purple-700", mainNav === "templates" && "text-purple-700")}
        >
          Templates
        </a>
        <a
          href="#"
          onClick={() => navigate("definition")}
          className={cn("text-sm font-medium transition-colors hover:text-purple-700", mainNav === "definition" && "text-purple-700")}
        >
          Definition
        </a>
      </nav>
    </div>
  </div>;
}