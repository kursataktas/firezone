import KbSidebar from "@/components/KbSidebar";
import Alert from "@/components/DocsAlert";

export default function Layout({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex">
      <KbSidebar />
      <main className="p-4 pt-20 -ml-64 md:ml-0 lg:mx-auto">
        <div className="px-4">
          <article className="max-w-screen-md format format-sm md:format-md lg:format-lg format-firezone">
            {children}
          </article>
        </div>
      </main>
    </div>
  );
}
