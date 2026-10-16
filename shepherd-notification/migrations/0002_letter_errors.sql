-- Last generation error of a held letter (workbench or storage failure, unreadable source).
ALTER TABLE notification.letters ADD COLUMN last_error text;
