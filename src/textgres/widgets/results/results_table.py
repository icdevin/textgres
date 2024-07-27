from textgres.widgets.data_table import TextgresDataTable

class ResultsTable(TextgresDataTable):
  def on_mount(self):
    self.zebra_stripes = True