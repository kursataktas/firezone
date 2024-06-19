export default function Entry({
  version,
  date = new Date(),
  children,
}: {
  version: string;
  date: Date;
  children: React.ReactNode;
}) {
  const options: Intl.DateTimeFormatOptions = {
    timeZone: "UTC",
    year: "numeric",
    month: "long",
    day: "numeric",
  };
  const utcDateString = date.toLocaleDateString("en-US", options);
  return (
    <tr className="border-t">
      <td className="px-2 py-1 sm:px-3 sm:py-1.5 md:px-4 md:py-2 lg:px-6 lg:py-4">
        {version}
      </td>
      <td className="min-w-36 px-2 py-1 sm:px-3 sm:py-1.5 md:px-4 md:py-2 lg:px-6 lg:py-4">
        <time dateTime={date.toDateString()}>{utcDateString}</time>
      </td>
      <td className="px-2 py-1 sm:px-3 sm:py-1.5 md:px-4 md:py-2 lg:px-6 lg:py-4">
        {children}
      </td>
    </tr>
  );
}
